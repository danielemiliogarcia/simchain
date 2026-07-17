//! Single-mutation job coordinator, persistence, events, abort, worker/network
//! leases, and restart recovery.

use crate::apply::{apply_with_context, ApplyContext, ApplyRequest};
use crate::backend::{
    ChainBackend, MiningControlBackend, NetworkControlBackend, SpamControlBackend,
};
use crate::control_state::{ControlState, ControlStateStore};
use crate::faucet_job::{
    eligible_total, select_inputs, FaucetBackend, FaucetInput, FaucetPreflight,
    PreparedFaucetTransaction,
};
use crate::faucet_store::{FaucetStore, StoredFaucetTransfer};
use crate::job_store::JobStore;
use crate::network_job::NetworkActionBackend;
use crate::reorg_job::{ReorgExecution, ReorgExecutor, ReorgRecoveryContext};
use crate::scenario_job::ScenarioActionBackend;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use simchain_common::control_api::{
    AbortJobResponse, CheckpointState, CleanupState, ComponentState, DegradeJobRequest, ErrorCode,
    FaucetDeliveryState, FaucetJobRequest, FaucetOutput, FaucetSource, FaucetSourceNode,
    FaucetStatusResponse, FaucetTransfer, FaucetWalletStatus, JobCheckpoint, JobCheckpointResponse,
    JobCleanup, JobCreatedResponse, JobDetail, JobEvent, JobEventsResponse, JobFailure, JobKind,
    JobLease, JobListResponse, JobState, JobSummary, MineJobRequest, PartitionJobRequest,
    ReleaseCheckpointRequest, ReorgJobRequest, ScenarioStepStatus, SpamBurstJobRequest,
    FAUCET_MAX_OUTPUTS, FAUCET_MAX_TX_VBYTES, FAUCET_PRIORITY_DELTA_SATS,
    FAUCET_PRIORITY_DOMINANCE_FACTOR,
};
use simchain_common::internal_api::{
    LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest, NetworkImpairment,
    NetworkLeaseReleaseRequest, NetworkLeaseRequest, PauseLease,
};
use simchain_common::live_tuning::{self, ServiceScope};
use simchain_reorg::{ReorgObserver, ReorgPhase, ReorgProgress};
use simchain_scenario_engine::{
    CheckpointStep, ComponentExpectation, FaucetScenarioOutput, MinerNode, NetworkNode, Scenario,
    ScenarioActions, ScenarioComponent, ScenarioControl, ScenarioProgress, ScenarioProgressPhase,
    Step, WaitCondition, WaitTxStep,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const JOB_SCHEMA_VERSION: u32 = 2;
const MAX_JOB_HISTORY: usize = 100;
const EVENT_RING_CAPACITY: usize = 2_048;
const DEFAULT_LEASE_TTL_SECS: u64 = 120;
const FAUCET_SUBMIT_TIMEOUT_SECS: u64 = 30;
const MAX_EVENT_PAGE: usize = 500;

fn default_next_checkpoint_generation() -> u64 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredJob {
    detail: JobDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<String>,
    request_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    faucet_recovery: Option<FaucetRecoveryContext>,
    #[serde(default)]
    reorg_recovery: ReorgRecoveryContext,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FaucetPhase {
    #[default]
    Validated,
    InputsSelected,
    InputsLocked,
    Prepared,
    Node2Prioritized,
    Node3Prioritized,
    Node2Submitted,
    Node3Submitted,
    Armed,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct FaucetRecoveryContext {
    phase: FaucetPhase,
    normalized_request: Option<FaucetJobRequest>,
    source: Option<FaucetSourceNode>,
    wallet_name: Option<String>,
    selected_inputs: Vec<FaucetInput>,
    input_sats: Option<u64>,
    change_sats: Option<u64>,
    raw_tx_hex: Option<String>,
    txid: Option<String>,
    desired_priority_delta_sats: i64,
    node2_prioritized: bool,
    node3_prioritized: bool,
    node2_submitted: bool,
    node3_submitted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredJobV1 {
    detail: JobDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<String>,
    request_fingerprint: String,
    #[serde(default)]
    reorg_recovery: ReorgRecoveryContext,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedJobsV1 {
    schema_version: u32,
    next_event_sequence: u64,
    #[serde(default = "default_next_checkpoint_generation")]
    next_checkpoint_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_job_id: Option<String>,
    jobs: Vec<StoredJobV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedJobs {
    schema_version: u32,
    next_event_sequence: u64,
    #[serde(default = "default_next_checkpoint_generation")]
    next_checkpoint_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_job_id: Option<String>,
    jobs: Vec<StoredJob>,
}

impl Default for PersistedJobs {
    fn default() -> Self {
        Self {
            schema_version: JOB_SCHEMA_VERSION,
            next_event_sequence: 1,
            next_checkpoint_generation: 1,
            active_job_id: None,
            jobs: Vec::new(),
        }
    }
}

fn load_and_migrate_jobs(store: &JobStore) -> anyhow::Result<PersistedJobs> {
    let Some(value) = store.load_optional::<Value>()? else {
        return Ok(PersistedJobs::default());
    };
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("job index has no schema_version"))?;
    match version {
        1 => {
            let old: PersistedJobsV1 = serde_json::from_value(value)?;
            anyhow::ensure!(old.schema_version == 1, "invalid v1 job schema marker");
            let migrated = PersistedJobs {
                schema_version: JOB_SCHEMA_VERSION,
                next_event_sequence: old.next_event_sequence,
                next_checkpoint_generation: old.next_checkpoint_generation,
                active_job_id: old.active_job_id,
                jobs: old
                    .jobs
                    .into_iter()
                    .map(|job| StoredJob {
                        detail: job.detail,
                        idempotency_key: job.idempotency_key,
                        request_fingerprint: job.request_fingerprint,
                        faucet_recovery: None,
                        reorg_recovery: job.reorg_recovery,
                    })
                    .collect(),
            };
            store.save(&migrated)?;
            Ok(migrated)
        }
        2 => serde_json::from_value(value).map_err(Into::into),
        future => anyhow::bail!("unsupported job schema {future} (expected {JOB_SCHEMA_VERSION})"),
    }
}

struct ManagerState {
    persisted: PersistedJobs,
    events: VecDeque<JobEvent>,
    aborts: HashMap<String, Arc<AtomicBool>>,
    recovering: HashSet<String>,
    recovery_errors: HashMap<String, String>,
    delivery_recovering: bool,
}

#[derive(Clone, Debug)]
pub struct JobManagerError {
    pub code: ErrorCode,
    pub message: String,
    pub active_job_id: Option<String>,
}

impl JobManagerError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            active_job_id: None,
        }
    }

    fn operation_in_progress(job_id: String) -> Self {
        Self {
            code: ErrorCode::OperationInProgress,
            message: format!("chain mutation job {job_id} is still active"),
            active_job_id: Some(job_id),
        }
    }
}

pub struct JobManager {
    store: JobStore,
    state: Mutex<ManagerState>,
    mining: Arc<dyn MiningControlBackend>,
    spam: Arc<dyn SpamControlBackend>,
    chain: Arc<dyn ChainBackend>,
    control_store: ControlStateStore,
    control_state: Arc<RwLock<ControlState>>,
    apply_lock: Arc<Mutex<()>>,
    network: Arc<dyn NetworkControlBackend>,
    network_actions: Arc<dyn NetworkActionBackend>,
    reorg: Arc<dyn ReorgExecutor>,
    scenario: Arc<dyn ScenarioActionBackend>,
    faucet: Arc<dyn FaucetBackend>,
    faucet_store: FaucetStore,
    faucet_settings: FaucetSettings,
    checkpoint_cv: Condvar,
    id_sequence: AtomicU64,
    lease_ttl_secs: u64,
}

#[derive(Clone, Debug)]
pub struct FaucetSettings {
    pub node2_wallet_name: String,
    pub node3_wallet_name: String,
    pub wallet_reserve_sats: u64,
    pub max_request_sats: u64,
    pub explorer_url: String,
}

pub struct JobDependencies {
    pub mining: Arc<dyn MiningControlBackend>,
    pub spam: Arc<dyn SpamControlBackend>,
    pub chain: Arc<dyn ChainBackend>,
    pub control_store: ControlStateStore,
    pub control_state: Arc<RwLock<ControlState>>,
    pub apply_lock: Arc<Mutex<()>>,
    pub network: Arc<dyn NetworkControlBackend>,
    pub reorg: Arc<dyn ReorgExecutor>,
    pub scenario: Arc<dyn ScenarioActionBackend>,
    pub network_actions: Arc<dyn NetworkActionBackend>,
    pub faucet: Arc<dyn FaucetBackend>,
    pub faucet_settings: FaucetSettings,
}

impl JobManager {
    pub fn open(
        state_dir: &std::path::Path,
        dependencies: JobDependencies,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open_with_ttl(state_dir, dependencies, DEFAULT_LEASE_TTL_SECS)
    }

    fn open_with_ttl(
        state_dir: &std::path::Path,
        dependencies: JobDependencies,
        lease_ttl_secs: u64,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(lease_ttl_secs > 0, "job lease TTL must be positive");
        let store = JobStore::open(state_dir)?;
        let mut persisted = load_and_migrate_jobs(&store)?;
        let faucet_store = FaucetStore::open(state_dir)?;

        let mut all_events = Vec::new();
        for job in &persisted.jobs {
            all_events.extend(store.read_events(&job.detail.summary.id)?);
        }
        all_events.sort_by_key(|event| event.sequence);
        if let Some(maximum) = all_events.last().map(|event| event.sequence) {
            persisted.next_event_sequence = persisted.next_event_sequence.max(maximum + 1);
        }
        let events = all_events
            .into_iter()
            .rev()
            .take(EVENT_RING_CAPACITY)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        let recovery_job = persisted.active_job_id.clone();
        if let Some(job_id) = recovery_job.as_deref() {
            let job = find_stored_mut(&mut persisted, job_id).ok_or_else(|| {
                anyhow::anyhow!("active job {job_id} is missing from job history")
            })?;
            if !job.detail.summary.state.is_terminal() {
                job.detail.summary.state = JobState::Interrupted;
                job.detail.summary.phase = "recovering_owned_resources".to_string();
                job.detail.summary.ended_at_ms = Some(now_ms());
                job.detail.summary.cleanup.state = CleanupState::Running;
                job.detail.failure = Some(JobFailure {
                    code: "control_plane_restarted".to_string(),
                    message: "control plane restarted before the job reached a terminal state"
                        .to_string(),
                });
            }
            store.save(&persisted)?;
        }

        let manager = Arc::new(Self {
            store,
            state: Mutex::new(ManagerState {
                persisted,
                events,
                aborts: HashMap::new(),
                recovering: HashSet::new(),
                recovery_errors: HashMap::new(),
                delivery_recovering: false,
            }),
            mining: dependencies.mining,
            spam: dependencies.spam,
            chain: dependencies.chain,
            control_store: dependencies.control_store,
            control_state: dependencies.control_state,
            apply_lock: dependencies.apply_lock,
            network: dependencies.network,
            reorg: dependencies.reorg,
            scenario: dependencies.scenario,
            network_actions: dependencies.network_actions,
            faucet: dependencies.faucet,
            faucet_store,
            faucet_settings: dependencies.faucet_settings,
            checkpoint_cv: Condvar::new(),
            id_sequence: AtomicU64::new(1),
            lease_ttl_secs,
        });
        if let Some(job_id) = recovery_job {
            manager.emit_best_effort(
                &job_id,
                "restart_recovery",
                "recovering_owned_resources",
                "previous active job was marked interrupted; recovering owned leases",
                None,
            );
            manager.spawn_recovery(job_id);
        }
        manager.spawn_delivery_guard();
        Ok(manager)
    }

    pub fn ensure_idle(&self) -> Result<(), JobManagerError> {
        let state = self.state.lock().expect("job manager lock");
        if state.delivery_recovering {
            return Err(JobManagerError::operation_in_progress(
                "faucet-delivery-recovery".to_string(),
            ));
        }
        match state.persisted.active_job_id.clone() {
            Some(job_id) => Err(JobManagerError::operation_in_progress(job_id)),
            None => Ok(()),
        }
    }

    pub fn active_summary(&self) -> Option<JobSummary> {
        let state = self.state.lock().expect("job manager lock");
        let job_id = state.persisted.active_job_id.as_deref()?;
        find_stored(&state.persisted, job_id).map(|job| job.detail.summary.clone())
    }

    pub fn has_pending_faucet(&self) -> bool {
        self.faucet_store.pending().is_some()
    }

    pub fn faucet_transfer(&self, txid: &str) -> Option<FaucetTransfer> {
        self.faucet_store.get(txid)
    }

    pub fn faucet_status(&self) -> FaucetStatusResponse {
        let preflight = self.faucet.preflight();
        let (available, last_probe_error, wallets) = match preflight {
            Ok(preflight) => {
                let wallets = [
                    (
                        FaucetSourceNode::Node2,
                        &self.faucet_settings.node2_wallet_name,
                        &preflight.node2_inputs,
                    ),
                    (
                        FaucetSourceNode::Node3,
                        &self.faucet_settings.node3_wallet_name,
                        &preflight.node3_inputs,
                    ),
                ]
                .into_iter()
                .map(|(source, wallet_name, inputs)| {
                    let total = eligible_total(inputs).unwrap_or(0);
                    FaucetWalletStatus {
                        source,
                        wallet_name: wallet_name.clone(),
                        eligible_confirmed_sats: total,
                        available_after_reserve_sats: total
                            .saturating_sub(self.faucet_settings.wallet_reserve_sats),
                        error: None,
                    }
                })
                .collect();
                (true, None, wallets)
            }
            Err(error) => {
                let message = error.to_string();
                let wallets = [
                    (
                        FaucetSourceNode::Node2,
                        &self.faucet_settings.node2_wallet_name,
                    ),
                    (
                        FaucetSourceNode::Node3,
                        &self.faucet_settings.node3_wallet_name,
                    ),
                ]
                .into_iter()
                .map(|(source, wallet_name)| FaucetWalletStatus {
                    source,
                    wallet_name: wallet_name.clone(),
                    eligible_confirmed_sats: 0,
                    available_after_reserve_sats: 0,
                    error: Some(message.clone()),
                })
                .collect();
                (false, Some(message), wallets)
            }
        };
        FaucetStatusResponse {
            available,
            last_probe_error,
            max_request_sats: self.faucet_settings.max_request_sats,
            max_outputs: FAUCET_MAX_OUTPUTS,
            wallet_reserve_sats: self.faucet_settings.wallet_reserve_sats,
            max_tx_vbytes: FAUCET_MAX_TX_VBYTES,
            priority_delta_sats: FAUCET_PRIORITY_DELTA_SATS,
            wallets,
            pending_transfer: self.faucet_store.pending().map(|record| record.public),
            recent_transfers: self.faucet_store.recent(),
        }
    }

    fn ensure_pending_faucet_armed(&self) -> Result<(), JobManagerError> {
        let Some(pending) = self.faucet_store.pending() else {
            return Ok(());
        };
        for node in [FaucetSourceNode::Node2, FaucetSourceNode::Node3] {
            let verification = self
                .faucet
                .verify_miner(node, &pending.public.txid)
                .map_err(|error| {
                    JobManagerError::new(
                        ErrorCode::FaucetDeliveryPending,
                        format!(
                            "manual mining is waiting for faucet delivery recovery on {}: {error}",
                            node.as_str()
                        ),
                    )
                })?;
            if verification.base_fee_sats != 0
                || verification.modified_fee_sats != FAUCET_PRIORITY_DELTA_SATS as u64
                || verification.fee_delta_sats != FAUCET_PRIORITY_DELTA_SATS
                || verification.ancestor_count != 1
            {
                return Err(JobManagerError::new(
                    ErrorCode::FaucetDeliveryPending,
                    format!(
                        "manual mining is blocked while faucet delivery is re-armed on {}",
                        node.as_str()
                    ),
                ));
            }
        }
        Ok(())
    }

    pub fn start_faucet(
        self: &Arc<Self>,
        request: FaucetJobRequest,
        idempotency_key: Option<String>,
    ) -> Result<JobCreatedResponse, JobManagerError> {
        let request = normalize_faucet_request(request, self.faucet_settings.max_request_sats)?;
        let request_value = serde_json::to_value(&request).map_err(internal_error)?;
        let fingerprint = serde_json::to_string(&request).map_err(internal_error)?;
        let idempotency_key = normalize_required_idempotency_key(idempotency_key)?;
        let abort = Arc::new(AtomicBool::new(false));

        let job_id = {
            let mut state = self.state.lock().expect("job manager lock");
            if let Some(existing) = state
                .persisted
                .jobs
                .iter()
                .find(|job| job.idempotency_key.as_deref() == Some(idempotency_key.as_str()))
            {
                if existing.detail.summary.kind != JobKind::Faucet
                    || existing.request_fingerprint != fingerprint
                {
                    return Err(JobManagerError::new(
                        ErrorCode::ValidationFailed,
                        "idempotency key was already used for a different job request",
                    ));
                }
                return Ok(JobCreatedResponse {
                    job_id: existing.detail.summary.id.clone(),
                    state: existing.detail.summary.state,
                    reused: true,
                });
            }
            if let Some(active) = state.persisted.active_job_id.clone() {
                return Err(JobManagerError::operation_in_progress(active));
            }
            if state.delivery_recovering {
                return Err(JobManagerError::operation_in_progress(
                    "faucet-delivery-recovery".to_string(),
                ));
            }
            if self.faucet_store.pending().is_some() {
                return Err(JobManagerError::new(
                    ErrorCode::FaucetDeliveryPending,
                    "a faucet transfer is already armed and awaiting confirmation",
                ));
            }

            let job_id = self.next_job_id();
            let created_at_ms = now_ms();
            state.persisted.jobs.push(StoredJob {
                detail: JobDetail {
                    summary: JobSummary {
                        id: job_id.clone(),
                        kind: JobKind::Faucet,
                        state: JobState::Starting,
                        phase: "validating_request".to_string(),
                        created_at_ms,
                        started_at_ms: None,
                        ended_at_ms: None,
                        cleanup: JobCleanup::default(),
                    },
                    request: request_value,
                    leases: Vec::new(),
                    current_step: None,
                    checkpoints: Vec::new(),
                    result: None,
                    failure: None,
                },
                idempotency_key: Some(idempotency_key),
                request_fingerprint: fingerprint,
                faucet_recovery: Some(FaucetRecoveryContext {
                    phase: FaucetPhase::Validated,
                    normalized_request: Some(request.clone()),
                    desired_priority_delta_sats: FAUCET_PRIORITY_DELTA_SATS,
                    ..FaucetRecoveryContext::default()
                }),
                reorg_recovery: ReorgRecoveryContext::default(),
            });
            state.persisted.active_job_id = Some(job_id.clone());
            state.aborts.insert(job_id.clone(), abort.clone());
            self.trim_history_locked(&mut state)
                .map_err(internal_error)?;
            self.store.save(&state.persisted).map_err(internal_error)?;
            job_id
        };

        if let Err(error) = self.emit(
            &job_id,
            "created",
            "validating_request",
            "faucet job accepted",
            Some(json!({"source": request.source, "output_count": request.outputs.len()})),
        ) {
            self.fail_before_thread(&job_id, error.to_string());
            return Err(internal_error(error));
        }
        let manager = self.clone();
        let thread_job_id = job_id.clone();
        let spawn = thread::Builder::new()
            .name(format!("faucet-{job_id}"))
            .spawn(move || {
                let panic_manager = manager.clone();
                let panic_job_id = thread_job_id.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    manager.run_faucet_job(thread_job_id, request, abort)
                }));
                if outcome.is_err() {
                    panic_manager.handle_executor_panic(&panic_job_id);
                }
            });
        if let Err(error) = spawn {
            self.fail_before_thread(&job_id, format!("failed to start faucet thread: {error}"));
            return Err(JobManagerError::new(
                ErrorCode::Internal,
                format!("failed to start faucet thread: {error}"),
            ));
        }
        Ok(JobCreatedResponse {
            job_id,
            state: JobState::Starting,
            reused: false,
        })
    }

    pub fn start_reorg(
        self: &Arc<Self>,
        request: ReorgJobRequest,
        idempotency_key: Option<String>,
        use_raw_tx_spam: bool,
    ) -> Result<JobCreatedResponse, JobManagerError> {
        let request = normalize_reorg_request(request)?;
        let request_value = serde_json::to_value(&request).map_err(internal_error)?;
        let fingerprint = serde_json::to_string(&request).map_err(internal_error)?;
        let idempotency_key = normalize_idempotency_key(idempotency_key)?;
        let abort = Arc::new(AtomicBool::new(false));

        let job_id = {
            let mut state = self.state.lock().expect("job manager lock");
            if let Some(key) = idempotency_key.as_deref() {
                if let Some(existing) = state
                    .persisted
                    .jobs
                    .iter()
                    .find(|job| job.idempotency_key.as_deref() == Some(key))
                {
                    if existing.detail.summary.kind != JobKind::Reorg
                        || existing.request_fingerprint != fingerprint
                    {
                        return Err(JobManagerError::new(
                            ErrorCode::ValidationFailed,
                            "idempotency key was already used for a different job request",
                        ));
                    }
                    return Ok(JobCreatedResponse {
                        job_id: existing.detail.summary.id.clone(),
                        state: existing.detail.summary.state,
                        reused: true,
                    });
                }
            }
            if let Some(active) = state.persisted.active_job_id.clone() {
                return Err(JobManagerError::operation_in_progress(active));
            }
            if state.delivery_recovering {
                return Err(JobManagerError::operation_in_progress(
                    "faucet-delivery-recovery".to_string(),
                ));
            }
            if self.faucet_store.pending().is_some() {
                return Err(JobManagerError::new(
                    ErrorCode::FaucetDeliveryPending,
                    "reorg is blocked until the armed faucet transfer confirms",
                ));
            }

            let job_id = self.next_job_id();
            let created_at_ms = now_ms();
            state.persisted.jobs.push(StoredJob {
                detail: JobDetail {
                    summary: JobSummary {
                        id: job_id.clone(),
                        kind: JobKind::Reorg,
                        state: JobState::Starting,
                        phase: "starting".to_string(),
                        created_at_ms,
                        started_at_ms: None,
                        ended_at_ms: None,
                        cleanup: JobCleanup::default(),
                    },
                    request: request_value,
                    leases: Vec::new(),
                    current_step: None,
                    checkpoints: Vec::new(),
                    result: None,
                    failure: None,
                },
                idempotency_key,
                request_fingerprint: fingerprint,
                // Conservative from acceptance onward: on restart, first
                // prove target/witness tips agree before releasing leases.
                // The invalidating progress callback fills the block hash
                // before the non-idempotent RPC is issued.
                faucet_recovery: None,
                reorg_recovery: ReorgRecoveryContext {
                    mutation_may_have_occurred: true,
                    request: Some(request.clone()),
                    invalidated_block_hash: None,
                },
            });
            state.persisted.active_job_id = Some(job_id.clone());
            state.aborts.insert(job_id.clone(), abort.clone());
            self.trim_history_locked(&mut state)
                .map_err(internal_error)?;
            self.store.save(&state.persisted).map_err(internal_error)?;
            job_id
        };

        if let Err(error) = self.emit(
            &job_id,
            "created",
            "starting",
            "reorg job accepted",
            Some(json!({"request": request})),
        ) {
            self.fail_before_thread(&job_id, error.to_string());
            return Err(internal_error(error));
        }

        let manager = self.clone();
        let thread_job_id = job_id.clone();
        let spawn = thread::Builder::new()
            .name(format!("reorg-{job_id}"))
            .spawn(move || {
                let panic_manager = manager.clone();
                let panic_job_id = thread_job_id.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    manager.run_reorg_job(thread_job_id, request, use_raw_tx_spam, abort)
                }));
                if outcome.is_err() {
                    panic_manager.handle_executor_panic(&panic_job_id);
                }
            });
        if let Err(error) = spawn {
            self.fail_before_thread(&job_id, format!("failed to start job thread: {error}"));
            return Err(JobManagerError::new(
                ErrorCode::Internal,
                format!("failed to start job thread: {error}"),
            ));
        }

        Ok(JobCreatedResponse {
            job_id,
            state: JobState::Starting,
            reused: false,
        })
    }

    pub fn start_scenario(
        self: &Arc<Self>,
        yaml: String,
        idempotency_key: Option<String>,
        use_raw_tx_spam: bool,
    ) -> Result<JobCreatedResponse, JobManagerError> {
        if yaml.trim().is_empty() || yaml.len() > 1024 * 1024 {
            return Err(JobManagerError::new(
                ErrorCode::ValidationFailed,
                "scenario YAML must be non-empty and no larger than 1 MiB",
            ));
        }
        let scenario = Scenario::parse(&yaml).map_err(|error| {
            JobManagerError::new(
                ErrorCode::ValidationFailed,
                format!("invalid scenario: {error:#}"),
            )
        })?;
        let fingerprint = serde_json::to_string(&scenario).map_err(internal_error)?;
        let request_value = serde_json::to_value(&scenario).map_err(internal_error)?;
        let idempotency_key = normalize_idempotency_key(idempotency_key)?;
        let abort = Arc::new(AtomicBool::new(false));
        let checkpoints = scenario
            .steps
            .iter()
            .enumerate()
            .filter_map(|(index, step)| match step {
                Step::Checkpoint { checkpoint } => Some(JobCheckpoint {
                    name: checkpoint.name.clone(),
                    generation: 0,
                    state: CheckpointState::Pending,
                    pause: checkpoint.pause,
                    timeout_secs: checkpoint.timeout_secs,
                    step_index: index + 1,
                    arrived_at_ms: None,
                    released_at_ms: None,
                    live_summary: None,
                }),
                _ => None,
            })
            .collect();

        let job_id = {
            let mut state = self.state.lock().expect("job manager lock");
            if let Some(key) = idempotency_key.as_deref() {
                if let Some(existing) = state
                    .persisted
                    .jobs
                    .iter()
                    .find(|job| job.idempotency_key.as_deref() == Some(key))
                {
                    if existing.detail.summary.kind != JobKind::Scenario
                        || existing.request_fingerprint != fingerprint
                    {
                        return Err(JobManagerError::new(
                            ErrorCode::ValidationFailed,
                            "idempotency key was already used for a different job request",
                        ));
                    }
                    return Ok(JobCreatedResponse {
                        job_id: existing.detail.summary.id.clone(),
                        state: existing.detail.summary.state,
                        reused: true,
                    });
                }
            }
            if let Some(active) = state.persisted.active_job_id.clone() {
                return Err(JobManagerError::operation_in_progress(active));
            }
            if state.delivery_recovering {
                return Err(JobManagerError::operation_in_progress(
                    "faucet-delivery-recovery".to_string(),
                ));
            }
            if self.faucet_store.pending().is_some() {
                return Err(JobManagerError::new(
                    ErrorCode::FaucetDeliveryPending,
                    "scenario is blocked until the armed faucet transfer confirms",
                ));
            }

            let job_id = self.next_job_id();
            state.persisted.jobs.push(StoredJob {
                detail: JobDetail {
                    summary: JobSummary {
                        id: job_id.clone(),
                        kind: JobKind::Scenario,
                        state: JobState::Starting,
                        phase: "starting".to_string(),
                        created_at_ms: now_ms(),
                        started_at_ms: None,
                        ended_at_ms: None,
                        cleanup: JobCleanup::default(),
                    },
                    request: request_value,
                    leases: Vec::new(),
                    current_step: None,
                    checkpoints,
                    result: None,
                    failure: None,
                },
                idempotency_key,
                request_fingerprint: fingerprint,
                faucet_recovery: None,
                reorg_recovery: ReorgRecoveryContext::default(),
            });
            state.persisted.active_job_id = Some(job_id.clone());
            state.aborts.insert(job_id.clone(), abort.clone());
            self.trim_history_locked(&mut state)
                .map_err(internal_error)?;
            self.store.save(&state.persisted).map_err(internal_error)?;
            job_id
        };

        if let Err(error) = self.emit(
            &job_id,
            "created",
            "starting",
            "scenario job accepted",
            Some(json!({"steps": scenario.steps.len()})),
        ) {
            self.fail_before_thread(&job_id, error.to_string());
            return Err(internal_error(error));
        }

        let manager = self.clone();
        let thread_job_id = job_id.clone();
        let spawn = thread::Builder::new()
            .name(format!("scenario-{job_id}"))
            .spawn(move || {
                let panic_manager = manager.clone();
                let panic_job_id = thread_job_id.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    manager.run_scenario_job(thread_job_id, scenario, use_raw_tx_spam, abort)
                }));
                if outcome.is_err() {
                    panic_manager.handle_executor_panic(&panic_job_id);
                }
            });
        if let Err(error) = spawn {
            self.fail_before_thread(&job_id, format!("failed to start job thread: {error}"));
            return Err(JobManagerError::new(
                ErrorCode::Internal,
                format!("failed to start job thread: {error}"),
            ));
        }

        Ok(JobCreatedResponse {
            job_id,
            state: JobState::Starting,
            reused: false,
        })
    }

    pub fn start_mine(
        self: &Arc<Self>,
        request: MineJobRequest,
        idempotency_key: Option<String>,
    ) -> Result<JobCreatedResponse, JobManagerError> {
        let (request, node) = normalize_mine_request(request)?;
        self.ensure_pending_faucet_armed()?;
        let request_value = serde_json::to_value(&request).map_err(internal_error)?;
        let (created, abort) = self.reserve_action_job(
            JobKind::Mine,
            request_value,
            idempotency_key,
            "manual mine job accepted",
        )?;
        let Some(abort) = abort else {
            return Ok(created);
        };
        let manager = self.clone();
        let job_id = created.job_id.clone();
        let thread_job_id = job_id.clone();
        let spawn = thread::Builder::new()
            .name(format!("mine-{job_id}"))
            .spawn(move || {
                let panic_manager = manager.clone();
                let panic_job_id = thread_job_id.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    manager.run_mine_job(thread_job_id, node, request.blocks, abort)
                }));
                if outcome.is_err() {
                    panic_manager.handle_executor_panic(&panic_job_id);
                }
            });
        if let Err(error) = spawn {
            self.fail_before_thread(&job_id, format!("failed to start job thread: {error}"));
            return Err(JobManagerError::new(
                ErrorCode::Internal,
                format!("failed to start job thread: {error}"),
            ));
        }
        Ok(created)
    }

    pub fn start_spam_burst(
        self: &Arc<Self>,
        request: SpamBurstJobRequest,
        idempotency_key: Option<String>,
    ) -> Result<JobCreatedResponse, JobManagerError> {
        let (request, node) = normalize_spam_burst_request(request)?;
        let request_value = serde_json::to_value(&request).map_err(internal_error)?;
        let (created, abort) = self.reserve_action_job(
            JobKind::SpamBurst,
            request_value,
            idempotency_key,
            "spam burst job accepted",
        )?;
        let Some(abort) = abort else {
            return Ok(created);
        };
        let manager = self.clone();
        let job_id = created.job_id.clone();
        let thread_job_id = job_id.clone();
        let spawn = thread::Builder::new()
            .name(format!("spam-burst-{job_id}"))
            .spawn(move || {
                let panic_manager = manager.clone();
                let panic_job_id = thread_job_id.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    manager.run_spam_burst_job(
                        thread_job_id,
                        node,
                        request.txs,
                        request.outputs_per_tx,
                        abort,
                    )
                }));
                if outcome.is_err() {
                    panic_manager.handle_executor_panic(&panic_job_id);
                }
            });
        if let Err(error) = spawn {
            self.fail_before_thread(&job_id, format!("failed to start job thread: {error}"));
            return Err(JobManagerError::new(
                ErrorCode::Internal,
                format!("failed to start job thread: {error}"),
            ));
        }
        Ok(created)
    }

    pub fn start_partition(
        self: &Arc<Self>,
        request: PartitionJobRequest,
        idempotency_key: Option<String>,
    ) -> Result<JobCreatedResponse, JobManagerError> {
        let (request, node) = normalize_partition_request(request)?;
        let request_value = serde_json::to_value(&request).map_err(internal_error)?;
        let (created, abort) = self.reserve_action_job(
            JobKind::Partition,
            request_value,
            idempotency_key,
            "partition job accepted",
        )?;
        let Some(abort) = abort else {
            return Ok(created);
        };
        let manager = self.clone();
        let job_id = created.job_id.clone();
        let thread_job_id = job_id.clone();
        let spawn = thread::Builder::new()
            .name(format!("partition-{job_id}"))
            .spawn(move || {
                let panic_manager = manager.clone();
                let panic_job_id = thread_job_id.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    manager.run_partition_job(
                        thread_job_id,
                        node,
                        request.main_blocks,
                        request.isolated_blocks,
                        abort,
                    )
                }));
                if outcome.is_err() {
                    panic_manager.handle_executor_panic(&panic_job_id);
                }
            });
        if let Err(error) = spawn {
            self.fail_before_thread(&job_id, format!("failed to start job thread: {error}"));
            return Err(JobManagerError::new(
                ErrorCode::Internal,
                format!("failed to start job thread: {error}"),
            ));
        }
        Ok(created)
    }

    pub fn start_degrade(
        self: &Arc<Self>,
        request: DegradeJobRequest,
        idempotency_key: Option<String>,
    ) -> Result<JobCreatedResponse, JobManagerError> {
        let (request, node) = normalize_degrade_request(request)?;
        let request_value = serde_json::to_value(&request).map_err(internal_error)?;
        let (created, abort) = self.reserve_action_job(
            JobKind::Degrade,
            request_value,
            idempotency_key,
            "network degradation job accepted",
        )?;
        let Some(abort) = abort else {
            return Ok(created);
        };
        let manager = self.clone();
        let job_id = created.job_id.clone();
        let thread_job_id = job_id.clone();
        let spawn = thread::Builder::new()
            .name(format!("degrade-{job_id}"))
            .spawn(move || {
                let panic_manager = manager.clone();
                let panic_job_id = thread_job_id.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    manager.run_degrade_job(thread_job_id, node, request, abort)
                }));
                if outcome.is_err() {
                    panic_manager.handle_executor_panic(&panic_job_id);
                }
            });
        if let Err(error) = spawn {
            self.fail_before_thread(&job_id, format!("failed to start job thread: {error}"));
            return Err(JobManagerError::new(
                ErrorCode::Internal,
                format!("failed to start job thread: {error}"),
            ));
        }
        Ok(created)
    }

    fn reserve_action_job(
        self: &Arc<Self>,
        kind: JobKind,
        request: Value,
        idempotency_key: Option<String>,
        message: &str,
    ) -> Result<(JobCreatedResponse, Option<Arc<AtomicBool>>), JobManagerError> {
        let fingerprint = serde_json::to_string(&request).map_err(internal_error)?;
        let idempotency_key = normalize_idempotency_key(idempotency_key)?;
        let abort = Arc::new(AtomicBool::new(false));
        let job_id = {
            let mut state = self.state.lock().expect("job manager lock");
            if let Some(key) = idempotency_key.as_deref() {
                if let Some(existing) = state
                    .persisted
                    .jobs
                    .iter()
                    .find(|job| job.idempotency_key.as_deref() == Some(key))
                {
                    if existing.detail.summary.kind != kind
                        || existing.request_fingerprint != fingerprint
                    {
                        return Err(JobManagerError::new(
                            ErrorCode::ValidationFailed,
                            "idempotency key was already used for a different job request",
                        ));
                    }
                    return Ok((
                        JobCreatedResponse {
                            job_id: existing.detail.summary.id.clone(),
                            state: existing.detail.summary.state,
                            reused: true,
                        },
                        None,
                    ));
                }
            }
            if let Some(active) = state.persisted.active_job_id.clone() {
                return Err(JobManagerError::operation_in_progress(active));
            }
            if state.delivery_recovering {
                return Err(JobManagerError::operation_in_progress(
                    "faucet-delivery-recovery".to_string(),
                ));
            }
            if self.faucet_store.pending().is_some()
                && !matches!(kind, JobKind::Mine | JobKind::SpamBurst)
            {
                return Err(JobManagerError::new(
                    ErrorCode::FaucetDeliveryPending,
                    format!(
                        "{} is blocked until the armed faucet transfer confirms",
                        kind.as_str()
                    ),
                ));
            }
            let job_id = self.next_job_id();
            state.persisted.jobs.push(StoredJob {
                detail: JobDetail {
                    summary: JobSummary {
                        id: job_id.clone(),
                        kind,
                        state: JobState::Starting,
                        phase: "starting".to_string(),
                        created_at_ms: now_ms(),
                        started_at_ms: None,
                        ended_at_ms: None,
                        cleanup: JobCleanup::default(),
                    },
                    request: request.clone(),
                    leases: Vec::new(),
                    current_step: None,
                    checkpoints: Vec::new(),
                    result: None,
                    failure: None,
                },
                idempotency_key,
                request_fingerprint: fingerprint,
                faucet_recovery: None,
                reorg_recovery: ReorgRecoveryContext::default(),
            });
            state.persisted.active_job_id = Some(job_id.clone());
            state.aborts.insert(job_id.clone(), abort.clone());
            self.trim_history_locked(&mut state)
                .map_err(internal_error)?;
            self.store.save(&state.persisted).map_err(internal_error)?;
            job_id
        };
        if let Err(error) = self.emit(
            &job_id,
            "created",
            "starting",
            message,
            Some(json!({"request": request})),
        ) {
            self.fail_before_thread(&job_id, error.to_string());
            return Err(internal_error(error));
        }
        Ok((
            JobCreatedResponse {
                job_id,
                state: JobState::Starting,
                reused: false,
            },
            Some(abort),
        ))
    }

    pub fn list(&self) -> JobListResponse {
        let state = self.state.lock().expect("job manager lock");
        JobListResponse {
            active_job_id: state.persisted.active_job_id.clone(),
            jobs: state
                .persisted
                .jobs
                .iter()
                .rev()
                .map(|job| job.detail.summary.clone())
                .collect(),
        }
    }

    pub fn get(&self, job_id: &str) -> Result<JobDetail, JobManagerError> {
        let state = self.state.lock().expect("job manager lock");
        find_stored(&state.persisted, job_id)
            .map(|job| job.detail.clone())
            .ok_or_else(|| JobManagerError::new(ErrorCode::JobNotFound, "job not found"))
    }

    pub fn events(
        &self,
        job_id: Option<&str>,
        after: u64,
        limit: usize,
    ) -> Result<JobEventsResponse, JobManagerError> {
        let limit = limit.clamp(1, MAX_EVENT_PAGE);
        let events = if let Some(job_id) = job_id {
            // Keep the manager lock while reading JSONL. Event writers hold
            // the same lock across append+fsync, so readers can never observe
            // a partially written line or race file rotation.
            let state = self.state.lock().expect("job manager lock");
            if find_stored(&state.persisted, job_id).is_none() {
                return Err(JobManagerError::new(
                    ErrorCode::JobNotFound,
                    "job not found",
                ));
            }
            self.store.read_events(job_id).map_err(internal_error)?
        } else {
            self.state
                .lock()
                .expect("job manager lock")
                .events
                .iter()
                .cloned()
                .collect()
        };
        let events: Vec<JobEvent> = events
            .into_iter()
            .filter(|event| event.sequence > after)
            .take(limit)
            .collect();
        let next_sequence = events.last().map(|event| event.sequence).unwrap_or(after);
        Ok(JobEventsResponse {
            events,
            next_sequence,
        })
    }

    pub fn abort(&self, job_id: &str) -> Result<AbortJobResponse, JobManagerError> {
        let mut state = self.state.lock().expect("job manager lock");
        let current = find_stored(&state.persisted, job_id)
            .ok_or_else(|| JobManagerError::new(ErrorCode::JobNotFound, "job not found"))?
            .detail
            .summary
            .state;
        if current.is_terminal() {
            return Ok(AbortJobResponse {
                job_id: job_id.to_string(),
                state: current,
            });
        }
        if let Some(abort) = state.aborts.get(job_id) {
            abort.store(true, Ordering::Release);
        }
        let job = find_stored_mut(&mut state.persisted, job_id).expect("job checked above");
        job.detail.summary.state = JobState::AbortRequested;
        self.store.save(&state.persisted).map_err(internal_error)?;
        drop(state);
        self.emit_best_effort(
            job_id,
            "abort_requested",
            "abort_requested",
            "cooperative abort requested",
            None,
        );
        self.checkpoint_cv.notify_all();
        Ok(AbortJobResponse {
            job_id: job_id.to_string(),
            state: JobState::AbortRequested,
        })
    }

    pub fn checkpoint(
        &self,
        job_id: &str,
        name: &str,
    ) -> Result<JobCheckpointResponse, JobManagerError> {
        let state = self.state.lock().expect("job manager lock");
        let job = find_stored(&state.persisted, job_id)
            .ok_or_else(|| JobManagerError::new(ErrorCode::JobNotFound, "job not found"))?;
        if job.detail.summary.kind != JobKind::Scenario {
            return Err(JobManagerError::new(
                ErrorCode::JobNotFound,
                "scenario checkpoint not found",
            ));
        }
        let checkpoint = job
            .detail
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.name == name)
            .cloned()
            .ok_or_else(|| {
                JobManagerError::new(ErrorCode::JobNotFound, "scenario checkpoint not found")
            })?;
        Ok(JobCheckpointResponse {
            job_id: job_id.to_string(),
            checkpoint,
        })
    }

    pub fn release_checkpoint(
        &self,
        job_id: &str,
        name: &str,
        request: ReleaseCheckpointRequest,
    ) -> Result<JobCheckpointResponse, JobManagerError> {
        let checkpoint = {
            let mut state = self.state.lock().expect("job manager lock");
            let is_active = state.persisted.active_job_id.as_deref() == Some(job_id);
            let job = find_stored_mut(&mut state.persisted, job_id)
                .ok_or_else(|| JobManagerError::new(ErrorCode::JobNotFound, "job not found"))?;
            let job_is_terminal = job.detail.summary.state.is_terminal();
            let checkpoint = job
                .detail
                .checkpoints
                .iter_mut()
                .find(|checkpoint| checkpoint.name == name)
                .ok_or_else(|| {
                    JobManagerError::new(ErrorCode::JobNotFound, "scenario checkpoint not found")
                })?;
            if checkpoint.generation == 0 || checkpoint.state == CheckpointState::Pending {
                return Err(JobManagerError::new(
                    ErrorCode::CheckpointConflict,
                    "checkpoint has not been reached",
                ));
            }
            if checkpoint.generation != request.generation {
                return Err(JobManagerError::new(
                    ErrorCode::CheckpointConflict,
                    format!(
                        "stale checkpoint generation {} (current {})",
                        request.generation, checkpoint.generation
                    ),
                ));
            }
            if !checkpoint.pause {
                return Err(JobManagerError::new(
                    ErrorCode::CheckpointConflict,
                    "milestone-only checkpoint does not pause",
                ));
            }
            match checkpoint.state {
                CheckpointState::Released => {
                    return Ok(JobCheckpointResponse {
                        job_id: job_id.to_string(),
                        checkpoint: checkpoint.clone(),
                    })
                }
                CheckpointState::Reached => {
                    if !is_active || job_is_terminal {
                        return Err(JobManagerError::new(
                            ErrorCode::CheckpointConflict,
                            "checkpoint occurrence is no longer active",
                        ));
                    }
                    checkpoint.state = CheckpointState::Released;
                    checkpoint.released_at_ms = Some(now_ms());
                }
                CheckpointState::TimedOut => {
                    return Err(JobManagerError::new(
                        ErrorCode::CheckpointConflict,
                        "checkpoint already timed out",
                    ));
                }
                CheckpointState::Pending => unreachable!("pending handled above"),
            }
            let checkpoint = checkpoint.clone();
            if job.detail.summary.state == JobState::WaitingAtCheckpoint {
                job.detail.summary.state = JobState::Running;
                job.detail.summary.phase = "checkpoint_released".to_string();
                if let Some(step) = job.detail.current_step.as_mut() {
                    step.state = "running".to_string();
                }
            }
            self.store.save(&state.persisted).map_err(internal_error)?;
            checkpoint
        };
        self.emit_best_effort(
            job_id,
            "checkpoint_released",
            "checkpoint_released",
            &format!("checkpoint '{}' released", checkpoint.name),
            Some(json!({
                "name": checkpoint.name,
                "generation": checkpoint.generation
            })),
        );
        self.checkpoint_cv.notify_all();
        Ok(JobCheckpointResponse {
            job_id: job_id.to_string(),
            checkpoint,
        })
    }

    fn run_faucet_job(
        self: Arc<Self>,
        job_id: String,
        request: FaucetJobRequest,
        abort: Arc<AtomicBool>,
    ) {
        if abort.load(Ordering::Acquire) {
            self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_before_start",
                None,
                None,
                successful_cleanup(),
            );
            return;
        }
        self.set_running(&job_id, "faucet_preflight");
        let initial = match self.faucet.preflight() {
            Ok(preflight) => preflight,
            Err(error) => {
                self.finish_job(
                    &job_id,
                    JobState::Failed,
                    "faucet_preflight_failed",
                    None,
                    Some(JobFailure {
                        code: "faucet_unavailable".to_string(),
                        message: error.to_string(),
                    }),
                    successful_cleanup(),
                );
                return;
            }
        };
        self.emit_best_effort(
            &job_id,
            "faucet_preflight_completed",
            "faucet_preflight",
            "faucet wallets, miners, and common tip are ready",
            Some(json!({"height": initial.height, "best_hash": initial.best_hash})),
        );

        self.set_phase(&job_id, "acquiring_mining_lease");
        let lease = match self.acquire_scenario_lease(
            &job_id,
            "mining",
            "faucet exact-zero transaction arming",
            1,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                self.finish_job(
                    &job_id,
                    JobState::Failed,
                    "mining_lease_failed",
                    None,
                    Some(JobFailure {
                        code: "faucet_unavailable".to_string(),
                        message: error.to_string(),
                    }),
                    successful_cleanup(),
                );
                return;
            }
        };
        self.emit_best_effort(
            &job_id,
            "mining_lease_acquired",
            "acquiring_mining_lease",
            "mining paused at a safe point",
            None,
        );
        let renewer = match OwnedLeaseRenewer::start(
            self.clone(),
            job_id.clone(),
            abort.clone(),
            self.lease_ttl_secs,
        ) {
            Ok(renewer) => renewer,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, vec![lease], abort);
                return;
            }
        };

        let outcome = self.execute_faucet(&job_id, &request, &abort);
        let context = self.faucet_context(&job_id).unwrap_or_default();
        let submitted = context.node2_submitted || context.node3_submitted;
        let stop_error = renewer.stop().err().map(|error| error.to_string());
        let mut extra_cleanup_errors = Vec::new();

        if outcome.is_err() || abort.load(Ordering::Acquire) {
            if let Some(txid) = context.txid.as_deref() {
                for node in [FaucetSourceNode::Node2, FaucetSourceNode::Node3] {
                    if let Err(error) = self.faucet.set_priority(node, txid, 0) {
                        extra_cleanup_errors.push(format!("{} priority: {error}", node.as_str()));
                    }
                }
            }
            if let Some(source) = context.source {
                if !context.selected_inputs.is_empty() {
                    if let Err(error) = self.faucet.unlock_inputs(source, &context.selected_inputs)
                    {
                        extra_cleanup_errors.push(format!("wallet input unlock: {error}"));
                    }
                }
            }
        }
        let mut cleanup = self.cleanup_leases(&job_id, &[lease], false, stop_error);
        cleanup.errors.extend(extra_cleanup_errors);
        if !cleanup.errors.is_empty() {
            cleanup.state = CleanupState::Failed;
        }

        match outcome {
            Ok(transfer) if abort.load(Ordering::Acquire) => {
                let _ = self.faucet_store.mark_failed(
                    &transfer.txid,
                    FaucetDeliveryState::AbortedAfterSubmission,
                    "aborted after miner submission; transaction may still confirm".to_string(),
                );
                self.finish_job(
                    &job_id,
                    JobState::Aborted,
                    "aborted_after_submission",
                    serde_json::to_value(&transfer).ok(),
                    None,
                    cleanup,
                );
            }
            Ok(transfer) => {
                self.clear_faucet_job_recovery_material(&job_id);
                self.finish_job(
                    &job_id,
                    JobState::Succeeded,
                    "armed_for_next_block",
                    serde_json::to_value(&transfer).ok(),
                    None,
                    cleanup,
                );
            }
            Err(_error) if abort.load(Ordering::Acquire) => {
                if submitted {
                    if let Some(txid) = context.txid.as_deref() {
                        if let Some(transfer) = self.transfer_from_context(
                            &request,
                            &context,
                            initial.height,
                            initial.best_hash,
                            true,
                        ) {
                            let _ = self.faucet_store.arm(StoredFaucetTransfer {
                                public: transfer,
                                raw_tx_hex: context.raw_tx_hex.clone(),
                                selected_inputs: context.selected_inputs.clone(),
                            });
                            let _ = self.faucet_store.mark_failed(
                                txid,
                                FaucetDeliveryState::AbortedAfterSubmission,
                                "aborted after miner submission; transaction may still confirm"
                                    .to_string(),
                            );
                        }
                    }
                }
                self.finish_job(
                    &job_id,
                    JobState::Aborted,
                    if submitted {
                        "aborted_after_submission"
                    } else {
                        "aborted_safely"
                    },
                    context
                        .txid
                        .map(|txid| json!({"txid": txid, "may_still_confirm": submitted})),
                    None,
                    cleanup,
                );
            }
            Err(error) => self.finish_job(
                &job_id,
                JobState::Failed,
                "faucet_failed",
                context
                    .txid
                    .map(|txid| json!({"txid": txid, "may_still_confirm": submitted})),
                Some(JobFailure {
                    code: error.code.to_string(),
                    message: error.message,
                }),
                cleanup,
            ),
        }
    }

    fn execute_faucet(
        &self,
        job_id: &str,
        request: &FaucetJobRequest,
        abort: &AtomicBool,
    ) -> Result<FaucetTransfer, FaucetRunError> {
        if abort.load(Ordering::Acquire) {
            return Err(FaucetRunError::aborted());
        }
        self.set_phase(job_id, "selecting_faucet_inputs");
        let preflight = self
            .faucet
            .preflight()
            .map_err(FaucetRunError::unavailable)?;
        let total_sats = request
            .outputs
            .iter()
            .try_fold(0_u64, |total, output| total.checked_add(output.amount_sats))
            .ok_or_else(|| FaucetRunError::validation("output amount overflow"))?;
        let (source, candidates) = choose_faucet_source(
            request.source,
            &preflight,
            total_sats,
            self.faucet_settings.wallet_reserve_sats,
        )?;
        let selected = select_inputs(
            candidates,
            total_sats,
            self.faucet_settings.wallet_reserve_sats,
        )
        .map_err(FaucetRunError::insufficient)?;
        let wallet_name = self.wallet_name(source).to_string();
        self.update_faucet_context(job_id, |context| {
            context.phase = FaucetPhase::InputsSelected;
            context.source = Some(source);
            context.wallet_name = Some(wallet_name.clone());
            context.selected_inputs = selected.clone();
        })?;

        self.faucet
            .lock_inputs(source, &selected)
            .map_err(FaucetRunError::unavailable)?;
        self.update_faucet_context(job_id, |context| {
            context.phase = FaucetPhase::InputsLocked;
        })?;
        self.emit_best_effort(
            job_id,
            "faucet_inputs_locked",
            "selecting_faucet_inputs",
            "mature miner funds selected and locked",
            Some(json!({"source": source, "input_count": selected.len()})),
        );
        if abort.load(Ordering::Acquire) {
            return Err(FaucetRunError::aborted());
        }

        self.set_phase(job_id, "building_exact_zero_transaction");
        let prepared = self
            .faucet
            .prepare_transaction(source, &selected, &request.outputs)
            .map_err(FaucetRunError::validation_error)?;
        self.update_faucet_context(job_id, |context| {
            context.phase = FaucetPhase::Prepared;
            context.input_sats = Some(prepared.input_sats);
            context.change_sats = Some(prepared.change_sats);
            context.raw_tx_hex = Some(prepared.raw_tx_hex.clone());
            context.txid = Some(prepared.txid.clone());
        })?;
        self.emit_best_effort(
            job_id,
            "faucet_transaction_prepared",
            "building_exact_zero_transaction",
            "exact-zero transaction signed and durably prepared",
            Some(json!({"txid": prepared.txid, "vsize": prepared.vsize, "actual_fee_sats": 0})),
        );

        self.arm_prepared(job_id, &prepared, abort)?;
        let observer_unconfirmed = self
            .faucet
            .observer_contains_unconfirmed(&prepared.txid)
            .unwrap_or(false);
        let transfer = self
            .transfer_from_context(
                request,
                &self.faucet_context(job_id).unwrap_or_default(),
                preflight.height,
                preflight.best_hash,
                observer_unconfirmed,
            )
            .ok_or_else(|| FaucetRunError::internal("prepared faucet context is incomplete"))?;
        self.faucet_store
            .arm(StoredFaucetTransfer {
                public: transfer.clone(),
                raw_tx_hex: Some(prepared.raw_tx_hex.clone()),
                selected_inputs: selected.clone(),
            })
            .map_err(FaucetRunError::internal_error)?;
        self.update_faucet_context(job_id, |context| {
            context.phase = FaucetPhase::Armed;
        })?;
        self.emit_best_effort(
            job_id,
            "faucet_armed",
            "armed_for_next_block",
            "both miners verified; mining may resume",
            Some(json!({
                "txid": prepared.txid,
                "source": source,
                "output_count": request.outputs.len(),
                "total_sats": total_sats
            })),
        );
        Ok(transfer)
    }

    fn arm_prepared(
        &self,
        job_id: &str,
        prepared: &PreparedFaucetTransaction,
        abort: &AtomicBool,
    ) -> Result<(), FaucetRunError> {
        for (node, phase, context_phase) in [
            (
                FaucetSourceNode::Node2,
                "arming_node2",
                FaucetPhase::Node2Prioritized,
            ),
            (
                FaucetSourceNode::Node3,
                "arming_node3",
                FaucetPhase::Node3Prioritized,
            ),
        ] {
            self.set_phase(job_id, phase);
            let update = self
                .faucet
                .set_priority(node, &prepared.txid, FAUCET_PRIORITY_DELTA_SATS)
                .map_err(FaucetRunError::priority)?;
            self.update_faucet_context(job_id, |context| {
                context.phase = context_phase;
                match node {
                    FaucetSourceNode::Node2 => context.node2_prioritized = true,
                    FaucetSourceNode::Node3 => context.node3_prioritized = true,
                }
            })?;
            self.emit_best_effort(
                job_id,
                "faucet_priority_set",
                phase,
                &format!("{} virtual priority set", node.as_str()),
                Some(json!({
                    "node": node,
                    "desired_delta_sats": FAUCET_PRIORITY_DELTA_SATS,
                    "previous_delta_sats": update.previous_delta_sats
                })),
            );
        }
        for node in [FaucetSourceNode::Node2, FaucetSourceNode::Node3] {
            self.faucet
                .test_accept(node, &prepared.raw_tx_hex)
                .map_err(FaucetRunError::priority)?;
        }

        for node in [FaucetSourceNode::Node2, FaucetSourceNode::Node3] {
            let deadline = Instant::now() + Duration::from_secs(FAUCET_SUBMIT_TIMEOUT_SECS);
            let mut attempts = 0u64;
            loop {
                if abort.load(Ordering::Acquire) {
                    return Err(FaucetRunError::aborted());
                }
                attempts += 1;
                match self
                    .faucet
                    .submit(node, &prepared.raw_tx_hex, &prepared.txid)
                {
                    Ok(already_present) => {
                        self.update_faucet_context(job_id, |context| {
                            context.phase = match node {
                                FaucetSourceNode::Node2 => FaucetPhase::Node2Submitted,
                                FaucetSourceNode::Node3 => FaucetPhase::Node3Submitted,
                            };
                            match node {
                                FaucetSourceNode::Node2 => context.node2_submitted = true,
                                FaucetSourceNode::Node3 => context.node3_submitted = true,
                            }
                        })?;
                        self.emit_best_effort(
                            job_id,
                            "faucet_submission_accepted",
                            &format!("submitting_{}", node.as_str()),
                            &format!("{} accepted the exact faucet transaction", node.as_str()),
                            Some(json!({"node": node, "already_present": already_present})),
                        );
                        break;
                    }
                    Err(error) => {
                        if Instant::now() >= deadline {
                            return Err(FaucetRunError::priority(format!(
                                "{} did not accept the faucet transaction within {}s after {attempts} attempt(s); last error: {error}",
                                node.as_str(),
                                FAUCET_SUBMIT_TIMEOUT_SECS
                            )));
                        }
                        self.emit_best_effort(
                            job_id,
                            "faucet_submission_retry",
                            &format!("submitting_{}", node.as_str()),
                            &format!("{} submission retry: {error}", node.as_str()),
                            None,
                        );
                        thread::sleep(
                            Duration::from_secs(1)
                                .min(deadline.saturating_duration_since(Instant::now())),
                        );
                    }
                }
            }
        }

        self.set_phase(job_id, "verifying_next_block_priority");
        let spam_bound = self
            .spam
            .status()
            .map_err(FaucetRunError::unavailable)?
            .policy
            .max_generated_feerate_sat_vb()
            .ceil() as u64;
        for node in [FaucetSourceNode::Node2, FaucetSourceNode::Node3] {
            let verification = self
                .faucet
                .verify_miner(node, &prepared.txid)
                .map_err(FaucetRunError::priority)?;
            if verification.base_fee_sats != 0
                || verification.modified_fee_sats != FAUCET_PRIORITY_DELTA_SATS as u64
                || verification.fee_delta_sats != FAUCET_PRIORITY_DELTA_SATS
                || verification.vsize != prepared.vsize
                || verification.ancestor_count != 1
            {
                return Err(FaucetRunError::priority(format!(
                    "{} mempool verification failed: base={}, modified={}, delta={}, vsize={}, ancestors={}",
                    node.as_str(),
                    verification.base_fee_sats,
                    verification.modified_fee_sats,
                    verification.fee_delta_sats,
                    verification.vsize,
                    verification.ancestor_count
                )));
            }
            let competitor = verification
                .greatest_competing_feerate_sat_vb
                .max(verification.minimum_feerate_sat_vb)
                .max(spam_bound);
            let faucet_rate = FAUCET_PRIORITY_DELTA_SATS as u64 / prepared.vsize.max(1);
            let required = competitor.saturating_mul(FAUCET_PRIORITY_DOMINANCE_FACTOR);
            if faucet_rate < required {
                return Err(FaucetRunError::priority(format!(
                    "{} faucet modified feerate {faucet_rate} sat/vB is below required {required} sat/vB (competitor {competitor})",
                    node.as_str()
                )));
            }
            self.emit_best_effort(
                job_id,
                "faucet_miner_verified",
                "verifying_next_block_priority",
                &format!("{} mempool and priority verified", node.as_str()),
                Some(json!({
                    "node": node,
                    "base_fee_sats": verification.base_fee_sats,
                    "modified_fee_sats": verification.modified_fee_sats,
                    "vsize": verification.vsize
                })),
            );
        }
        Ok(())
    }

    fn update_faucet_context(
        &self,
        job_id: &str,
        update: impl FnOnce(&mut FaucetRecoveryContext),
    ) -> Result<(), FaucetRunError> {
        let mut state = self.state.lock().expect("job manager lock");
        let job = find_stored_mut(&mut state.persisted, job_id)
            .ok_or_else(|| FaucetRunError::internal("faucet job disappeared"))?;
        let context = job
            .faucet_recovery
            .as_mut()
            .ok_or_else(|| FaucetRunError::internal("faucet recovery context disappeared"))?;
        update(context);
        self.store
            .save(&state.persisted)
            .map_err(FaucetRunError::internal_error)
    }

    fn faucet_context(&self, job_id: &str) -> Option<FaucetRecoveryContext> {
        let state = self.state.lock().expect("job manager lock");
        find_stored(&state.persisted, job_id)?
            .faucet_recovery
            .clone()
    }

    fn clear_faucet_job_recovery_material(&self, job_id: &str) {
        let mut state = self.state.lock().expect("job manager lock");
        if let Some(context) = find_stored_mut(&mut state.persisted, job_id)
            .and_then(|job| job.faucet_recovery.as_mut())
        {
            context.raw_tx_hex = None;
            context.selected_inputs.clear();
        }
        if let Err(error) = self.store.save(&state.persisted) {
            tracing::error!(
                job_id,
                "failed to clear duplicate faucet recovery material: {error}"
            );
        }
    }

    fn wallet_name(&self, source: FaucetSourceNode) -> &str {
        match source {
            FaucetSourceNode::Node2 => &self.faucet_settings.node2_wallet_name,
            FaucetSourceNode::Node3 => &self.faucet_settings.node3_wallet_name,
        }
    }

    fn transfer_from_context(
        &self,
        request: &FaucetJobRequest,
        context: &FaucetRecoveryContext,
        height: u64,
        block_hash: String,
        observer_unconfirmed: bool,
    ) -> Option<FaucetTransfer> {
        let txid = context.txid.clone()?;
        let source = context.source?;
        let input_sats = context.input_sats?;
        let change_sats = context.change_sats?;
        let total_sats = input_sats.checked_sub(change_sats)?;
        let raw = context.raw_tx_hex.as_deref()?;
        let transaction: bitcoincore_rpc::bitcoin::Transaction =
            bitcoincore_rpc::bitcoin::consensus::encode::deserialize_hex(raw).ok()?;
        Some(FaucetTransfer {
            delivery_state: FaucetDeliveryState::Armed,
            txid: txid.clone(),
            source,
            wallet_name: context.wallet_name.clone()?,
            outputs: request.outputs.clone(),
            total_sats,
            change_sats,
            actual_fee_sats: 0,
            priority_delta_sats: FAUCET_PRIORITY_DELTA_SATS,
            vsize: transaction.vsize() as u64,
            armed_nodes: vec!["node2".to_string(), "node3".to_string()],
            visibility: "miner_only_unconfirmed".to_string(),
            armed_at_height: height,
            armed_at_block_hash: block_hash,
            armed_at_ms: now_ms(),
            confirmed_height: None,
            confirmed_block_hash: None,
            confirmed_at_ms: None,
            last_error: None,
            observer_unconfirmed,
            transfer_url: format!("/api/v1/faucet/transfers/{txid}"),
            explorer_url: format!(
                "{}/tx/{txid}",
                self.faucet_settings.explorer_url.trim_end_matches('/')
            ),
        })
    }

    fn run_reorg_job(
        self: Arc<Self>,
        job_id: String,
        request: ReorgJobRequest,
        use_raw_tx_spam: bool,
        abort: Arc<AtomicBool>,
    ) {
        if abort.load(Ordering::Acquire) {
            self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_before_start",
                None,
                None,
                JobCleanup {
                    state: CleanupState::Succeeded,
                    errors: Vec::new(),
                },
            );
            return;
        }
        self.set_running(&job_id, "acquiring_spam_lease");

        let mut leases = Vec::new();
        if let Err(error) = self.acquire_spam_lease(&job_id, &mut leases) {
            self.finish_failed_before_mutation(&job_id, error, leases, abort);
            return;
        }
        if abort.load(Ordering::Acquire) {
            self.finish_aborted_with_cleanup(&job_id, leases, false, None);
            return;
        }
        self.set_phase(&job_id, "acquiring_mining_lease");
        if let Err(error) = self.acquire_mining_lease(&job_id, &mut leases) {
            self.finish_failed_before_mutation(&job_id, error, leases, abort);
            return;
        }
        if abort.load(Ordering::Acquire) {
            self.finish_aborted_with_cleanup(&job_id, leases, false, None);
            return;
        }

        let renewer = match LeaseRenewer::start(
            self.clone(),
            job_id.clone(),
            abort.clone(),
            self.mining.clone(),
            self.spam.clone(),
            self.lease_ttl_secs,
        ) {
            Ok(renewer) => renewer,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, leases, abort);
                return;
            }
        };

        self.set_phase(&job_id, "executing_reorg");
        let observer = JobReorgObserver {
            manager: self.clone(),
            job_id: job_id.clone(),
            abort: abort.clone(),
            chain_changed: AtomicBool::new(false),
        };
        let execution = self.reorg.execute(&request, use_raw_tx_spam, &observer);
        let chain_changed = execution
            .as_ref()
            .map(|execution| execution.chain_changed)
            .unwrap_or(false)
            || observer.chain_changed.load(Ordering::Acquire);
        let stop_error = renewer.stop().err().map(|error| error.to_string());
        let cleanup = self.cleanup_leases(&job_id, &leases, chain_changed, stop_error);

        match execution {
            Ok(ReorgExecution {
                result, aborted, ..
            }) => {
                let state = if aborted || abort.load(Ordering::Acquire) {
                    JobState::Aborted
                } else {
                    JobState::Succeeded
                };
                let phase = if state == JobState::Aborted {
                    "aborted_safely"
                } else {
                    "succeeded"
                };
                self.finish_job(&job_id, state, phase, Some(result), None, cleanup);
            }
            Err(error) => self.finish_job(
                &job_id,
                JobState::Failed,
                "failed",
                None,
                Some(JobFailure {
                    code: "reorg_failed".to_string(),
                    message: error.to_string(),
                }),
                cleanup,
            ),
        }
    }

    fn run_scenario_job(
        self: Arc<Self>,
        job_id: String,
        scenario: Scenario,
        use_raw_tx_spam: bool,
        abort: Arc<AtomicBool>,
    ) {
        if abort.load(Ordering::Acquire) {
            self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_before_start",
                None,
                None,
                JobCleanup {
                    state: CleanupState::Succeeded,
                    errors: Vec::new(),
                },
            );
            return;
        }
        self.set_running(&job_id, "waiting_for_bootstrap");
        let actions = JobScenarioActions {
            manager: self.clone(),
            job_id: job_id.clone(),
            abort: abort.clone(),
            use_raw_tx_spam,
            runtime: Arc::new(Mutex::new(ScenarioRuntime::default())),
        };
        let renewer = match OwnedLeaseRenewer::start_for_scenario(
            self.clone(),
            job_id.clone(),
            abort.clone(),
            self.lease_ttl_secs,
            actions.runtime.clone(),
        ) {
            Ok(renewer) => renewer,
            Err(error) => {
                self.finish_job(
                    &job_id,
                    JobState::Failed,
                    "failed_to_start_lease_renewer",
                    None,
                    Some(JobFailure {
                        code: "lease_renewer_failed".to_string(),
                        message: error.to_string(),
                    }),
                    JobCleanup {
                        state: CleanupState::Succeeded,
                        errors: Vec::new(),
                    },
                );
                return;
            }
        };

        let bootstrap = self
            .scenario
            .wait_height(simchain_scenario_engine::BOOTSTRAP_HEIGHT, &actions);
        let result = match bootstrap {
            Ok(_) if !abort.load(Ordering::Acquire) => {
                self.set_phase(&job_id, "running_scenario");
                simchain_scenario_engine::run(&scenario, &actions, &actions)
            }
            Ok(_) => simchain_scenario_engine::ScenarioResult {
                success: false,
                aborted: true,
                executed_steps: 0,
                total_steps: scenario.steps.len(),
                duration_ms: 0,
                steps: Vec::new(),
                final_summary: self.scenario.live_summary().ok(),
                error: None,
            },
            Err(error) => simchain_scenario_engine::ScenarioResult {
                success: false,
                aborted: false,
                executed_steps: 0,
                total_steps: scenario.steps.len(),
                duration_ms: 0,
                steps: Vec::new(),
                final_summary: self.scenario.live_summary().ok(),
                error: Some(format!("bootstrap wait failed: {error:#}")),
            },
        };

        let stop_error = renewer.stop().err().map(|error| error.to_string());
        let chain_changed = actions
            .runtime
            .lock()
            .expect("scenario runtime lock")
            .chain_changed;
        let cleanup = self.cleanup_owned_leases(&job_id, chain_changed, stop_error);
        let result_value = serde_json::to_value(&result).ok();
        if result.aborted || abort.load(Ordering::Acquire) {
            self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_safely",
                result_value,
                None,
                cleanup,
            );
        } else if result.success {
            self.finish_job(
                &job_id,
                JobState::Succeeded,
                "succeeded",
                result_value,
                None,
                cleanup,
            );
        } else {
            self.finish_job(
                &job_id,
                JobState::Failed,
                "failed",
                result_value,
                Some(JobFailure {
                    code: "scenario_failed".to_string(),
                    message: result
                        .error
                        .clone()
                        .unwrap_or_else(|| "scenario failed".to_string()),
                }),
                cleanup,
            );
        }
    }

    fn run_mine_job(
        self: Arc<Self>,
        job_id: String,
        node: MinerNode,
        blocks: u64,
        abort: Arc<AtomicBool>,
    ) {
        if abort.load(Ordering::Acquire) {
            self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_before_start",
                None,
                None,
                JobCleanup {
                    state: CleanupState::Succeeded,
                    errors: Vec::new(),
                },
            );
            return;
        }
        self.set_running(&job_id, "acquiring_mining_lease");
        let lease = match self.acquire_scenario_lease(&job_id, "mining", "manual mine job", 1) {
            Ok(lease) => lease,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, Vec::new(), abort);
                return;
            }
        };
        let renewer = match OwnedLeaseRenewer::start(
            self.clone(),
            job_id.clone(),
            abort.clone(),
            self.lease_ttl_secs,
        ) {
            Ok(renewer) => renewer,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, vec![lease], abort);
                return;
            }
        };
        if abort.load(Ordering::Acquire) {
            let stop_error = renewer.stop().err().map(|error| error.to_string());
            self.finish_aborted_with_cleanup(&job_id, vec![lease], false, stop_error);
            return;
        }
        self.set_phase(&job_id, "mining_blocks");
        self.emit_best_effort(
            &job_id,
            "action_started",
            "mining_blocks",
            &format!("mining {blocks} block(s) on {node}"),
            None,
        );
        // A transport error can hide a successful non-idempotent RPC, so
        // release conservatively after the attempt.
        let result = self.scenario.mine(node, blocks);
        let stop_error = renewer.stop().err().map(|error| error.to_string());
        let cleanup = self.cleanup_leases(&job_id, &[lease], true, stop_error);
        match result {
            Ok(result) if abort.load(Ordering::Acquire) => self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_safely",
                Some(result),
                None,
                cleanup,
            ),
            Ok(result) => self.finish_job(
                &job_id,
                JobState::Succeeded,
                "succeeded",
                Some(result),
                None,
                cleanup,
            ),
            Err(error) => self.finish_job(
                &job_id,
                JobState::Failed,
                "failed",
                None,
                Some(JobFailure {
                    code: "mine_failed".to_string(),
                    message: error.to_string(),
                }),
                cleanup,
            ),
        }
    }

    fn run_spam_burst_job(
        self: Arc<Self>,
        job_id: String,
        node: MinerNode,
        txs: u64,
        outputs_per_tx: u64,
        abort: Arc<AtomicBool>,
    ) {
        if abort.load(Ordering::Acquire) {
            self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_before_start",
                None,
                None,
                JobCleanup {
                    state: CleanupState::Succeeded,
                    errors: Vec::new(),
                },
            );
            return;
        }
        self.set_running(&job_id, "acquiring_spam_lease");
        let lease = match self.acquire_scenario_lease(&job_id, "spam", "manual spam burst job", 1) {
            Ok(lease) => lease,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, Vec::new(), abort);
                return;
            }
        };
        let renewer = match OwnedLeaseRenewer::start(
            self.clone(),
            job_id.clone(),
            abort.clone(),
            self.lease_ttl_secs,
        ) {
            Ok(renewer) => renewer,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, vec![lease], abort);
                return;
            }
        };
        if abort.load(Ordering::Acquire) {
            let stop_error = renewer.stop().err().map(|error| error.to_string());
            self.finish_aborted_with_cleanup(&job_id, vec![lease], false, stop_error);
            return;
        }
        self.set_phase(&job_id, "submitting_spam_burst");
        self.emit_best_effort(
            &job_id,
            "action_started",
            "submitting_spam_burst",
            &format!("submitting {txs} transaction(s) from {node}"),
            None,
        );
        let control = SimpleJobControl {
            abort: abort.clone(),
        };
        let result = self
            .scenario
            .spam_burst(node, txs, outputs_per_tx, &control);
        let stop_error = renewer.stop().err().map(|error| error.to_string());
        let cleanup = self.cleanup_leases(&job_id, &[lease], false, stop_error);
        match result {
            Ok(result) if abort.load(Ordering::Acquire) => self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_safely",
                Some(result),
                None,
                cleanup,
            ),
            Ok(result) => self.finish_job(
                &job_id,
                JobState::Succeeded,
                "succeeded",
                Some(result),
                None,
                cleanup,
            ),
            Err(error) => self.finish_job(
                &job_id,
                JobState::Failed,
                "failed",
                None,
                Some(JobFailure {
                    code: "spam_burst_failed".to_string(),
                    message: error.to_string(),
                }),
                cleanup,
            ),
        }
    }

    fn run_partition_job(
        self: Arc<Self>,
        job_id: String,
        node: MinerNode,
        main_blocks: u64,
        isolated_blocks: u64,
        abort: Arc<AtomicBool>,
    ) {
        if abort.load(Ordering::Acquire) {
            self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_before_start",
                None,
                None,
                successful_cleanup(),
            );
            return;
        }
        self.set_running(&job_id, "validating_converged_start");
        if let Err(error) = self.network_actions.validate_ready_and_converged() {
            self.finish_failed_before_mutation(&job_id, error, Vec::new(), abort);
            return;
        }

        self.set_phase(&job_id, "acquiring_spam_lease");
        let spam = match self.acquire_scenario_lease(&job_id, "spam", "partition job", 1) {
            Ok(lease) => lease,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, Vec::new(), abort);
                return;
            }
        };
        self.set_phase(&job_id, "acquiring_mining_lease");
        let mining = match self.acquire_scenario_lease(&job_id, "mining", "partition job", 2) {
            Ok(lease) => lease,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, vec![spam], abort);
                return;
            }
        };
        let mut leases = vec![spam, mining];
        if abort.load(Ordering::Acquire) {
            self.finish_aborted_with_cleanup(&job_id, leases, false, None);
            return;
        }

        // Recheck after both workers reached their pause safe points so the
        // fork base cannot race continuous mining or spam reconciliation.
        if let Err(error) = self.network_actions.validate_ready_and_converged() {
            self.finish_failed_before_mutation(&job_id, error, leases, abort);
            return;
        }
        self.set_phase(&job_id, "applying_partition");
        let network = match self.acquire_network_lease(
            &job_id,
            node.short_name(),
            "deterministic partition job",
            NetworkImpairment::Partition {
                ingress_drop: true,
                egress_drop: true,
            },
            3,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, leases, abort);
                return;
            }
        };
        leases.push(network);
        let renewer = match OwnedLeaseRenewer::start(
            self.clone(),
            job_id.clone(),
            abort.clone(),
            self.lease_ttl_secs,
        ) {
            Ok(renewer) => renewer,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, leases, abort);
                return;
            }
        };
        let chain_changed = AtomicBool::new(false);
        let execution = self.execute_partition(
            &job_id,
            node,
            main_blocks,
            isolated_blocks,
            &SimpleJobControl {
                abort: abort.clone(),
            },
            &chain_changed,
        );
        let stop_error = renewer.stop().err().map(|error| error.to_string());
        let cleanup = self.cleanup_leases(
            &job_id,
            &leases,
            chain_changed.load(Ordering::Acquire),
            stop_error,
        );
        match execution {
            Ok(result) if abort.load(Ordering::Acquire) => self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_safely",
                Some(result),
                None,
                cleanup,
            ),
            Ok(result) => self.finish_job(
                &job_id,
                JobState::Succeeded,
                "succeeded",
                Some(result),
                None,
                cleanup,
            ),
            Err(error) => self.finish_job(
                &job_id,
                if abort.load(Ordering::Acquire) {
                    JobState::Aborted
                } else {
                    JobState::Failed
                },
                "failed",
                None,
                (!abort.load(Ordering::Acquire)).then(|| JobFailure {
                    code: "partition_failed".to_string(),
                    message: error.to_string(),
                }),
                cleanup,
            ),
        }
    }

    fn run_degrade_job(
        self: Arc<Self>,
        job_id: String,
        node: String,
        request: DegradeJobRequest,
        abort: Arc<AtomicBool>,
    ) {
        if abort.load(Ordering::Acquire) {
            self.finish_job(
                &job_id,
                JobState::Aborted,
                "aborted_before_start",
                None,
                None,
                successful_cleanup(),
            );
            return;
        }
        self.set_running(&job_id, "applying_network_degradation");
        let lease = match self.acquire_network_lease(
            &job_id,
            &node,
            "timed network degradation",
            NetworkImpairment::Netem {
                delay_ms: request.delay_ms,
                loss_pct: request.loss_pct,
            },
            1,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, Vec::new(), abort);
                return;
            }
        };
        let renewer = match OwnedLeaseRenewer::start(
            self.clone(),
            job_id.clone(),
            abort.clone(),
            self.lease_ttl_secs,
        ) {
            Ok(renewer) => renewer,
            Err(error) => {
                self.finish_failed_before_mutation(&job_id, error, vec![lease], abort);
                return;
            }
        };
        self.set_phase(&job_id, "observing_degraded_network");
        let started = Instant::now();
        let duration = Duration::from_secs(request.seconds);
        while started.elapsed() < duration && !abort.load(Ordering::Acquire) {
            thread::sleep(
                Duration::from_millis(100).min(duration.saturating_sub(started.elapsed())),
            );
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let stop_error = renewer.stop().err().map(|error| error.to_string());
        let cleanup = self.cleanup_leases(&job_id, &[lease], false, stop_error);
        let result = json!({
            "node": node,
            "delay_ms": request.delay_ms,
            "loss_pct": request.loss_pct,
            "requested_seconds": request.seconds,
            "elapsed_ms": elapsed_ms,
            "aborted": abort.load(Ordering::Acquire)
        });
        self.finish_job(
            &job_id,
            if abort.load(Ordering::Acquire) {
                JobState::Aborted
            } else {
                JobState::Succeeded
            },
            if abort.load(Ordering::Acquire) {
                "aborted_safely"
            } else {
                "succeeded"
            },
            Some(result),
            None,
            cleanup,
        );
    }

    fn acquire_spam_lease(&self, job_id: &str, leases: &mut Vec<JobLease>) -> anyhow::Result<()> {
        let lease = JobLease {
            component: "spam".to_string(),
            lease_id: format!("{job_id}-spam"),
            purpose: "reorg chain mutation".to_string(),
        };
        self.persist_lease_intent(job_id, lease.clone())?;
        self.spam.acquire_lease(LeaseRequest {
            lease_id: lease.lease_id.clone(),
            owner_job_id: job_id.to_string(),
            purpose: lease.purpose.clone(),
            ttl_secs: self.lease_ttl_secs,
            request_id: format!("{job_id}-spam-acquire"),
        })?;
        leases.push(lease.clone());
        self.acknowledge_lease(job_id, &lease);
        Ok(())
    }

    fn acquire_mining_lease(&self, job_id: &str, leases: &mut Vec<JobLease>) -> anyhow::Result<()> {
        let lease = JobLease {
            component: "mining".to_string(),
            lease_id: format!("{job_id}-mining"),
            purpose: "reorg chain mutation".to_string(),
        };
        self.persist_lease_intent(job_id, lease.clone())?;
        self.mining.acquire_lease(LeaseRequest {
            lease_id: lease.lease_id.clone(),
            owner_job_id: job_id.to_string(),
            purpose: lease.purpose.clone(),
            ttl_secs: self.lease_ttl_secs,
            request_id: format!("{job_id}-mining-acquire"),
        })?;
        leases.push(lease.clone());
        self.acknowledge_lease(job_id, &lease);
        Ok(())
    }

    fn acquire_scenario_lease(
        &self,
        job_id: &str,
        component: &str,
        purpose: &str,
        sequence: u64,
    ) -> anyhow::Result<JobLease> {
        let lease = JobLease {
            component: component.to_string(),
            lease_id: format!("{job_id}-{component}-{sequence}"),
            purpose: purpose.to_string(),
        };
        let request = LeaseRequest {
            lease_id: lease.lease_id.clone(),
            owner_job_id: job_id.to_string(),
            purpose: lease.purpose.clone(),
            ttl_secs: self.lease_ttl_secs,
            request_id: format!("{}-acquire", lease.lease_id),
        };
        self.persist_lease_intent(job_id, lease.clone())?;
        match component {
            "spam" => self.spam.acquire_lease(request)?,
            "mining" => self.mining.acquire_lease(request)?,
            other => anyhow::bail!("unknown lease component {other}"),
        };
        self.acknowledge_lease(job_id, &lease);
        Ok(lease)
    }

    fn acquire_network_lease(
        &self,
        job_id: &str,
        node: &str,
        purpose: &str,
        impairment: NetworkImpairment,
        sequence: u64,
    ) -> anyhow::Result<JobLease> {
        let lease = JobLease {
            component: format!("network:{node}"),
            lease_id: format!("{job_id}-network-{node}-{sequence}"),
            purpose: purpose.to_string(),
        };
        self.persist_lease_intent(job_id, lease.clone())?;
        self.network.acquire_lease(
            node,
            NetworkLeaseRequest {
                lease_id: lease.lease_id.clone(),
                owner_job_id: job_id.to_string(),
                purpose: purpose.to_string(),
                ttl_secs: self.lease_ttl_secs,
                request_id: format!("{}-acquire", lease.lease_id),
                impairment,
            },
        )?;
        self.acknowledge_lease(job_id, &lease);
        Ok(lease)
    }

    fn release_network_lease(&self, job_id: &str, lease: &JobLease) -> anyhow::Result<()> {
        let node = network_lease_node(lease)?;
        self.network.release_lease(
            node,
            &lease.lease_id,
            NetworkLeaseReleaseRequest {
                request_id: format!("{}-release", lease.lease_id),
            },
        )?;
        self.emit_best_effort(
            job_id,
            "lease_released",
            "network_healed",
            &format!("{node} network impairment lease released"),
            Some(json!({"lease": lease})),
        );
        Ok(())
    }

    fn execute_partition(
        &self,
        job_id: &str,
        node: MinerNode,
        main_blocks: u64,
        isolated_blocks: u64,
        control: &dyn ScenarioControl,
        chain_changed: &AtomicBool,
    ) -> anyhow::Result<Value> {
        let initial = self.network_actions.validate_ready_and_converged()?;
        self.set_phase(job_id, "disconnecting_partition_peers");
        self.network_actions.disconnect_target_peers(node)?;
        self.set_phase(job_id, "verifying_partition");
        let split = self.network_actions.wait_for_isolation(node, control)?;
        anyhow::ensure!(
            !control.abort_requested(),
            "partition aborted before branch mining"
        );

        let main = other_miner(node);
        self.set_phase(job_id, "mining_main_branch");
        chain_changed.store(true, Ordering::Release);
        let main_result = self.scenario.mine(main, main_blocks)?;
        let main_tip = result_hash(&main_result)?;
        self.set_phase(job_id, "mining_isolated_branch");
        let isolated_result = self.scenario.mine(node, isolated_blocks)?;
        let isolated_tip = result_hash(&isolated_result)?;
        let expected_tip = if main_blocks > isolated_blocks {
            main_tip.clone()
        } else {
            isolated_tip.clone()
        };

        self.set_phase(job_id, "healing_partition");
        let network_lease = self
            .get(job_id)
            .map_err(|error| anyhow::anyhow!(error.message))?
            .leases
            .into_iter()
            .rev()
            .find(|lease| lease.component == format!("network:{}", node.short_name()))
            .ok_or_else(|| anyhow::anyhow!("partition network lease disappeared"))?;
        self.release_network_lease(job_id, &network_lease)?;
        self.network_actions.reconnect_target(node)?;
        self.set_phase(job_id, "verifying_winning_tip");
        let final_snapshot = self
            .network_actions
            .wait_for_convergence(Some(&expected_tip), control)?;
        Ok(json!({
            "node": node.short_name(),
            "main_node": main.short_name(),
            "main_blocks": main_blocks,
            "isolated_blocks": isolated_blocks,
            "initial": initial,
            "split": split,
            "main_branch": main_result,
            "isolated_branch": isolated_result,
            "expected_tip": expected_tip,
            "final": final_snapshot
        }))
    }

    fn release_scenario_lease(
        &self,
        job_id: &str,
        lease: &JobLease,
        chain_changed: bool,
    ) -> anyhow::Result<()> {
        let request = LeaseReleaseRequest {
            request_id: format!("{}-release", lease.lease_id),
            chain_changed,
        };
        match lease.component.as_str() {
            "spam" => self.spam.release_lease(&lease.lease_id, request)?,
            "mining" => self.mining.release_lease(&lease.lease_id, request)?,
            other => anyhow::bail!("unknown lease component {other}"),
        };
        self.emit_best_effort(
            job_id,
            "lease_released",
            "lease_released",
            &format!("{} worker pause lease released", lease.component),
            Some(json!({"lease": lease, "chain_changed": chain_changed})),
        );
        Ok(())
    }

    fn persist_lease_intent(&self, job_id: &str, lease: JobLease) -> anyhow::Result<()> {
        let component = lease.component.clone();
        let mut state = self.state.lock().expect("job manager lock");
        let job = find_stored_mut(&mut state.persisted, job_id)
            .ok_or_else(|| anyhow::anyhow!("lease owner job {job_id} disappeared"))?;
        if !job
            .detail
            .leases
            .iter()
            .any(|existing| existing.lease_id == lease.lease_id)
        {
            job.detail.leases.push(lease.clone());
        }
        self.store.save(&state.persisted)?;
        drop(state);
        self.emit_best_effort(
            job_id,
            "lease_intent_recorded",
            "acquiring_lease",
            &format!("durably recorded intent to acquire {component} lease"),
            Some(json!({"lease": lease})),
        );
        Ok(())
    }

    fn acknowledge_lease(&self, job_id: &str, lease: &JobLease) {
        let component = lease.component.clone();
        self.emit_best_effort(
            job_id,
            "lease_acquired",
            &format!("{}_leased", component.replace(':', "_")),
            &format!("{component} acknowledged its owned lease"),
            Some(json!({"lease": lease})),
        );
    }

    fn cleanup_leases(
        &self,
        job_id: &str,
        leases: &[JobLease],
        chain_changed: bool,
        stop_error: Option<String>,
    ) -> JobCleanup {
        self.set_cleanup_running(job_id);
        let mut errors = stop_error.into_iter().collect::<Vec<_>>();
        // Network healing is first and is witnessed before either worker can
        // resume. Historical lease records are intentional: reconnect and
        // convergence are harmless if the happy path already healed.
        let network_leases: Vec<&JobLease> = leases
            .iter()
            .filter(|lease| lease.component.starts_with("network:"))
            .collect();
        for lease in &network_leases {
            match network_lease_node(lease) {
                Ok(node) => match self.network.status(node) {
                    Ok(status)
                        if status
                            .active_lease
                            .as_ref()
                            .is_some_and(|active| active.lease_id == lease.lease_id) =>
                    {
                        if let Err(error) = self.network.release_lease(
                            node,
                            &lease.lease_id,
                            NetworkLeaseReleaseRequest {
                                request_id: format!("{job_id}-{}-cleanup-release", lease.lease_id),
                            },
                        ) {
                            errors.push(format!(
                                "failed to heal {node} impairment {}: {error}",
                                lease.lease_id
                            ));
                        }
                    }
                    Ok(status) if status.active_lease.is_some() => errors.push(format!(
                        "{node} has a different active network lease during cleanup"
                    )),
                    Ok(_) => {}
                    Err(error) => errors.push(format!(
                        "failed to inspect {node} network impairment during cleanup: {error}"
                    )),
                },
                Err(error) => errors.push(error.to_string()),
            }
        }
        let mut healed_nodes = HashSet::new();
        for lease in &network_leases {
            let Ok(node_name) = network_lease_node(lease) else {
                continue;
            };
            if !healed_nodes.insert(node_name.to_string()) {
                continue;
            }
            if node_name == "node1" {
                continue;
            }
            match parse_miner_node(node_name) {
                Ok(node) => {
                    if let Err(error) = self.network_actions.reconnect_target(node) {
                        errors.push(format!("failed to reconnect {node_name}: {error}"));
                    }
                }
                Err(error) => errors.push(error.message),
            }
        }
        if !network_leases.is_empty() {
            if let Err(error) = self
                .network_actions
                .wait_for_convergence(None, &NeverAbortControl)
            {
                errors.push(format!(
                    "network healed but chain convergence failed: {error}"
                ));
            }
        }
        // Spam is released first so its chain-derived pools reconcile while
        // continuous mining is still held.
        for component in ["spam", "mining"] {
            for lease in leases.iter().filter(|lease| lease.component == component) {
                let request = LeaseReleaseRequest {
                    request_id: format!("{job_id}-{}-release", lease.lease_id),
                    chain_changed,
                };
                let result = match lease.component.as_str() {
                    "spam" => self.spam.release_lease(&lease.lease_id, request),
                    "mining" => self.mining.release_lease(&lease.lease_id, request),
                    other => Err(anyhow::anyhow!("unknown lease component {other}")),
                };
                if let Err(error) = result {
                    errors.push(format!(
                        "failed to release {} lease {}: {error}",
                        lease.component, lease.lease_id
                    ));
                }
            }
        }
        JobCleanup {
            state: if errors.is_empty() {
                CleanupState::Succeeded
            } else {
                CleanupState::Failed
            },
            errors,
        }
    }

    fn cleanup_owned_leases(
        &self,
        job_id: &str,
        chain_changed: bool,
        stop_error: Option<String>,
    ) -> JobCleanup {
        let leases = self.get(job_id).map(|job| job.leases).unwrap_or_default();
        self.cleanup_leases(job_id, &leases, chain_changed, stop_error)
    }

    fn renew_owned_leases(&self, job_id: &str, sequence: u64) -> anyhow::Result<()> {
        for lease in owned_leases(&self.spam.status()?.active_leases, job_id) {
            self.spam.renew_lease(
                &lease.lease_id,
                LeaseRenewRequest {
                    ttl_secs: self.lease_ttl_secs,
                    request_id: format!("{}-renew-{sequence}", lease.lease_id),
                },
            )?;
        }
        for lease in owned_leases(&self.mining.status()?.active_leases, job_id) {
            self.mining.renew_lease(
                &lease.lease_id,
                LeaseRenewRequest {
                    ttl_secs: self.lease_ttl_secs,
                    request_id: format!("{}-renew-{sequence}", lease.lease_id),
                },
            )?;
        }
        let network_nodes: HashSet<String> = self
            .get(job_id)
            .map_err(|error| anyhow::anyhow!(error.message))?
            .leases
            .into_iter()
            .filter_map(|lease| network_lease_node(&lease).ok().map(str::to_string))
            .collect();
        for node in network_nodes {
            let status = self.network.status(&node)?;
            if let Some(lease) = status
                .active_lease
                .filter(|lease| lease.owner_job_id == job_id)
            {
                self.network.renew_lease(
                    &node,
                    &lease.lease_id,
                    LeaseRenewRequest {
                        ttl_secs: self.lease_ttl_secs,
                        request_id: format!("{}-renew-{sequence}", lease.lease_id),
                    },
                )?;
            }
        }
        Ok(())
    }

    fn renew_scenario_runtime_leases(
        &self,
        sequence: u64,
        runtime: &Mutex<ScenarioRuntime>,
    ) -> anyhow::Result<()> {
        let (spam, mining, network) = {
            let runtime = runtime.lock().expect("scenario runtime lock");
            (
                runtime.spam_lease.clone(),
                runtime.mining_lease.clone(),
                runtime.network_leases.clone(),
            )
        };
        if let Some(lease) = spam {
            self.spam.renew_lease(
                &lease.lease_id,
                LeaseRenewRequest {
                    ttl_secs: self.lease_ttl_secs,
                    request_id: format!("{}-renew-{sequence}", lease.lease_id),
                },
            )?;
        }
        if let Some(lease) = mining {
            self.mining.renew_lease(
                &lease.lease_id,
                LeaseRenewRequest {
                    ttl_secs: self.lease_ttl_secs,
                    request_id: format!("{}-renew-{sequence}", lease.lease_id),
                },
            )?;
        }
        for lease in network {
            let node = network_lease_node(&lease)?;
            self.network.renew_lease(
                node,
                &lease.lease_id,
                LeaseRenewRequest {
                    ttl_secs: self.lease_ttl_secs,
                    request_id: format!("{}-renew-{sequence}", lease.lease_id),
                },
            )?;
        }
        Ok(())
    }

    fn finish_failed_before_mutation(
        self: &Arc<Self>,
        job_id: &str,
        error: anyhow::Error,
        leases: Vec<JobLease>,
        abort: Arc<AtomicBool>,
    ) {
        // A lease intent is persisted before the remote acquire call. If that
        // call took effect but its response was lost, the caller's local list
        // will not contain the lease even though cleanup still owns it.
        let mut cleanup_targets = self
            .get(job_id)
            .map(|detail| detail.leases)
            .unwrap_or_default();
        for lease in leases {
            if !cleanup_targets
                .iter()
                .any(|existing| existing.lease_id == lease.lease_id)
            {
                cleanup_targets.push(lease);
            }
        }
        let cleanup = self.cleanup_leases(job_id, &cleanup_targets, false, None);
        let state = if abort.load(Ordering::Acquire) {
            JobState::Aborted
        } else {
            JobState::Failed
        };
        self.finish_job(
            job_id,
            state,
            if state == JobState::Aborted {
                "aborted_before_mutation"
            } else {
                "failed_before_mutation"
            },
            None,
            (state == JobState::Failed).then(|| JobFailure {
                code: "lease_acquisition_failed".to_string(),
                message: error.to_string(),
            }),
            cleanup,
        );
    }

    fn finish_aborted_with_cleanup(
        self: &Arc<Self>,
        job_id: &str,
        leases: Vec<JobLease>,
        chain_changed: bool,
        stop_error: Option<String>,
    ) {
        let cleanup = self.cleanup_leases(job_id, &leases, chain_changed, stop_error);
        self.finish_job(
            job_id,
            JobState::Aborted,
            "aborted_before_mutation",
            None,
            None,
            cleanup,
        );
    }

    fn finish_job(
        self: &Arc<Self>,
        job_id: &str,
        final_state: JobState,
        phase: &str,
        result: Option<Value>,
        failure: Option<JobFailure>,
        cleanup: JobCleanup,
    ) {
        let cleanup_failed = cleanup.state == CleanupState::Failed;
        {
            let mut state = self.state.lock().expect("job manager lock");
            if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
                job.detail.summary.state = final_state;
                job.detail.summary.phase = phase.to_string();
                job.detail.summary.ended_at_ms = Some(now_ms());
                job.detail.summary.cleanup = cleanup.clone();
                job.detail.result = result;
                job.detail.failure = failure;
            }
            state.aborts.remove(job_id);
            if !cleanup_failed && state.persisted.active_job_id.as_deref() == Some(job_id) {
                state.persisted.active_job_id = None;
            }
            if let Err(error) = self.store.save(&state.persisted) {
                tracing::error!(job_id, "failed to persist terminal job state: {error}");
            }
        }
        self.emit_best_effort(
            job_id,
            "terminal",
            phase,
            &format!("job reached {}", final_state.as_str()),
            Some(json!({"state": final_state, "cleanup": cleanup})),
        );
        if cleanup_failed {
            self.spawn_recovery(job_id.to_string());
        }
    }

    fn set_running(&self, job_id: &str, phase: &str) {
        let mut state = self.state.lock().expect("job manager lock");
        if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
            if job.detail.summary.state != JobState::AbortRequested {
                job.detail.summary.state = JobState::Running;
            }
            job.detail.summary.phase = phase.to_string();
            job.detail.summary.started_at_ms.get_or_insert_with(now_ms);
            if let Err(error) = self.store.save(&state.persisted) {
                tracing::error!(job_id, "failed to persist running job: {error}");
            }
        }
        drop(state);
        self.emit_best_effort(job_id, "started", phase, "job executor started", None);
    }

    fn set_phase(&self, job_id: &str, phase: &str) {
        let mut state = self.state.lock().expect("job manager lock");
        if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
            job.detail.summary.phase = phase.to_string();
            if let Err(error) = self.store.save(&state.persisted) {
                tracing::error!(job_id, "failed to persist job phase: {error}");
            }
        }
    }

    fn set_cleanup_running(&self, job_id: &str) {
        let mut state = self.state.lock().expect("job manager lock");
        if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
            job.detail.summary.cleanup.state = CleanupState::Running;
            job.detail.summary.phase = "cleanup".to_string();
            if let Err(error) = self.store.save(&state.persisted) {
                tracing::error!(job_id, "failed to persist cleanup state: {error}");
            }
        }
    }

    fn emit(
        &self,
        job_id: &str,
        event_name: &str,
        phase: &str,
        message: &str,
        data: Option<Value>,
    ) -> anyhow::Result<JobEvent> {
        let mut state = self.state.lock().expect("job manager lock");
        anyhow::ensure!(
            find_stored(&state.persisted, job_id).is_some(),
            "event references an unknown job"
        );
        let sequence = state.persisted.next_event_sequence;
        state.persisted.next_event_sequence = sequence.saturating_add(1);
        let event = JobEvent {
            sequence,
            job_id: job_id.to_string(),
            timestamp_ms: now_ms(),
            event: event_name.to_string(),
            phase: phase.to_string(),
            message: message.to_string(),
            data,
        };
        self.store.append_event(&event)?;
        self.store.save(&state.persisted)?;
        state.events.push_back(event.clone());
        while state.events.len() > EVENT_RING_CAPACITY {
            state.events.pop_front();
        }
        Ok(event)
    }

    fn emit_best_effort(
        &self,
        job_id: &str,
        event_name: &str,
        phase: &str,
        message: &str,
        data: Option<Value>,
    ) {
        if let Err(error) = self.emit(job_id, event_name, phase, message, data) {
            tracing::error!(job_id, "failed to persist job event: {error}");
        }
    }

    fn fail_before_thread(self: &Arc<Self>, job_id: &str, message: String) {
        self.finish_job(
            job_id,
            JobState::Failed,
            "failed_to_start",
            None,
            Some(JobFailure {
                code: "job_thread_failed".to_string(),
                message,
            }),
            JobCleanup {
                state: CleanupState::Succeeded,
                errors: Vec::new(),
            },
        );
    }

    fn handle_executor_panic(self: &Arc<Self>, job_id: &str) {
        self.finish_job(
            job_id,
            JobState::Failed,
            "executor_panicked",
            None,
            Some(JobFailure {
                code: "executor_panicked".to_string(),
                message:
                    "job executor panicked; owned resources are being recovered conservatively"
                        .to_string(),
            }),
            JobCleanup {
                state: CleanupState::Failed,
                errors: vec!["executor panicked before explicit cleanup completed".to_string()],
            },
        );
    }

    fn trim_history_locked(&self, state: &mut ManagerState) -> anyhow::Result<()> {
        while state.persisted.jobs.len() > MAX_JOB_HISTORY {
            let removable = state
                .persisted
                .jobs
                .iter()
                .position(|job| job.detail.summary.state.is_terminal())
                .ok_or_else(|| anyhow::anyhow!("job history has no removable terminal record"))?;
            let removed = state.persisted.jobs.remove(removable);
            self.store.remove_events(&removed.detail.summary.id)?;
        }
        Ok(())
    }

    fn next_job_id(&self) -> String {
        let sequence = self.id_sequence.fetch_add(1, Ordering::Relaxed);
        format!("job-{}-{sequence}", now_ms())
    }

    fn spawn_delivery_guard(self: &Arc<Self>) {
        let manager = self.clone();
        if let Err(error) = thread::Builder::new()
            .name("faucet-delivery-guard".to_string())
            .spawn(move || loop {
                if let Err(error) = manager.poll_faucet_delivery() {
                    tracing::warn!("faucet delivery guard probe failed: {error}");
                }
                thread::sleep(Duration::from_secs(2));
            })
        {
            tracing::error!("failed to start faucet delivery guard: {error}");
        }
    }

    fn poll_faucet_delivery(self: &Arc<Self>) -> anyhow::Result<()> {
        let Some(pending) = self.faucet_store.pending() else {
            return self.poll_confirmed_faucet();
        };
        let txid = pending.public.txid.clone();
        if let Some(confirmation) = self.faucet.confirmation(&txid)? {
            self.faucet_store.mark_confirmed(
                &txid,
                confirmation.height,
                confirmation.block_hash.clone(),
                now_ms(),
            )?;
            self.emit_faucet_transfer_event(
                &txid,
                "faucet_confirmed",
                "confirmed",
                &format!(
                    "faucet transaction confirmed in block {}",
                    confirmation.height
                ),
                Some(json!({
                    "txid": txid,
                    "height": confirmation.height,
                    "block_hash": confirmation.block_hash
                })),
            );
            return Ok(());
        }

        let both_armed = [FaucetSourceNode::Node2, FaucetSourceNode::Node3]
            .into_iter()
            .all(|node| {
                self.faucet
                    .verify_miner(node, &txid)
                    .is_ok_and(|verification| {
                        verification.base_fee_sats == 0
                            && verification.modified_fee_sats == FAUCET_PRIORITY_DELTA_SATS as u64
                            && verification.fee_delta_sats == FAUCET_PRIORITY_DELTA_SATS
                            && verification.ancestor_count == 1
                    })
            });
        if both_armed {
            if pending.public.delivery_state == FaucetDeliveryState::Recovering {
                self.faucet_store.mark_armed(&txid)?;
            }
            return Ok(());
        }

        {
            let mut state = self.state.lock().expect("job manager lock");
            if state.delivery_recovering || state.persisted.active_job_id.is_some() {
                return Ok(());
            }
            state.delivery_recovering = true;
        }
        let result = self.recover_pending_transfer(&pending);
        self.state
            .lock()
            .expect("job manager lock")
            .delivery_recovering = false;
        result
    }

    fn poll_confirmed_faucet(&self) -> anyhow::Result<()> {
        let Some(transfer) = self.faucet_store.latest_confirmed() else {
            return Ok(());
        };
        let Some(confirmation) = self.faucet.confirmation(&transfer.txid)? else {
            let message = format!(
                "previously confirmed faucet transaction is no longer in the active chain at block {}",
                transfer.confirmed_block_hash.as_deref().unwrap_or("unknown")
            );
            self.faucet_store
                .mark_orphaned(&transfer.txid, message.clone())?;
            self.emit_faucet_transfer_event(
                &transfer.txid,
                "faucet_orphaned_after_confirmation",
                "orphaned_after_confirmation",
                &message,
                Some(json!({"txid": transfer.txid})),
            );
            return Ok(());
        };
        if transfer.confirmed_block_hash.as_deref() != Some(&confirmation.block_hash) {
            self.faucet_store.mark_confirmed(
                &transfer.txid,
                confirmation.height,
                confirmation.block_hash,
                now_ms(),
            )?;
        }
        Ok(())
    }

    fn recover_pending_transfer(&self, pending: &StoredFaucetTransfer) -> anyhow::Result<()> {
        let txid = &pending.public.txid;
        self.faucet_store.mark_recovering(txid, None)?;
        self.emit_faucet_transfer_event(
            txid,
            "faucet_recovery_started",
            "recovering",
            "re-arming saved faucet transaction after miner state changed",
            None,
        );
        let raw = pending.raw_tx_hex.as_deref().ok_or_else(|| {
            anyhow::anyhow!("pending faucet transfer has no recovery transaction")
        })?;
        if !self
            .faucet
            .inputs_unspent(pending.public.source, &pending.selected_inputs)?
        {
            let message = "prepared faucet inputs were spent by a conflicting transaction";
            self.faucet_store.mark_failed(
                txid,
                FaucetDeliveryState::DeliveryFailed,
                message.to_string(),
            )?;
            anyhow::bail!(message);
        }

        let lease_id = format!("faucet-delivery-{}", &txid[..txid.len().min(16)]);
        self.mining.acquire_lease(LeaseRequest {
            lease_id: lease_id.clone(),
            owner_job_id: "faucet-delivery-recovery".to_string(),
            purpose: "restore faucet next-block delivery".to_string(),
            ttl_secs: self.lease_ttl_secs,
            request_id: format!("{lease_id}-acquire-{}", now_ms()),
        })?;
        let recovery = (|| -> anyhow::Result<()> {
            for node in [FaucetSourceNode::Node2, FaucetSourceNode::Node3] {
                self.faucet
                    .set_priority(node, txid, FAUCET_PRIORITY_DELTA_SATS)?;
                self.faucet.test_accept(node, raw)?;
                self.faucet.submit(node, raw, txid)?;
            }
            for node in [FaucetSourceNode::Node2, FaucetSourceNode::Node3] {
                let verification = self.faucet.verify_miner(node, txid)?;
                anyhow::ensure!(
                    verification.base_fee_sats == 0
                        && verification.modified_fee_sats == FAUCET_PRIORITY_DELTA_SATS as u64
                        && verification.fee_delta_sats == FAUCET_PRIORITY_DELTA_SATS
                        && verification.ancestor_count == 1,
                    "{} did not restore the exact faucet priority invariant",
                    node.as_str()
                );
            }
            Ok(())
        })();
        let release = self.mining.release_lease(
            &lease_id,
            LeaseReleaseRequest {
                request_id: format!("{lease_id}-release-{}", now_ms()),
                chain_changed: false,
            },
        );
        if let Err(error) = recovery {
            let message = error.to_string();
            let _ = self
                .faucet_store
                .mark_recovering(txid, Some(message.clone()));
            let _ = release;
            return Err(error);
        }
        release?;
        self.faucet_store.mark_armed(txid)?;
        self.emit_faucet_transfer_event(
            txid,
            "faucet_recovery_completed",
            "armed_for_next_block",
            "saved faucet transaction is armed on both miners again",
            None,
        );
        Ok(())
    }

    fn emit_faucet_transfer_event(
        &self,
        txid: &str,
        event: &str,
        phase: &str,
        message: &str,
        data: Option<Value>,
    ) {
        let job_id = {
            let state = self.state.lock().expect("job manager lock");
            state
                .persisted
                .jobs
                .iter()
                .rev()
                .find(|job| {
                    job.faucet_recovery
                        .as_ref()
                        .and_then(|context| context.txid.as_deref())
                        == Some(txid)
                        || job
                            .detail
                            .result
                            .as_ref()
                            .and_then(|result| result.get("txid"))
                            .and_then(Value::as_str)
                            == Some(txid)
                })
                .map(|job| job.detail.summary.id.clone())
        };
        if let Some(job_id) = job_id {
            self.emit_best_effort(&job_id, event, phase, message, data);
        }
    }

    fn spawn_recovery(self: &Arc<Self>, job_id: String) {
        {
            let mut state = self.state.lock().expect("job manager lock");
            if !state.recovering.insert(job_id.clone()) {
                return;
            }
        }
        let manager = self.clone();
        let name = format!("recover-{job_id}");
        let recovery_id = job_id.clone();
        if let Err(error) = thread::Builder::new().name(name).spawn(move || {
            manager.recovery_loop(job_id);
        }) {
            tracing::error!("failed to start job recovery thread: {error}");
            self.state
                .lock()
                .expect("job manager lock")
                .recovering
                .remove(&recovery_id);
        }
    }

    fn recovery_loop(self: Arc<Self>, job_id: String) {
        loop {
            match self.recover_job_resources(&job_id) {
                Ok(()) => {
                    {
                        let mut state = self.state.lock().expect("job manager lock");
                        if let Some(job) = find_stored_mut(&mut state.persisted, &job_id) {
                            job.detail.summary.cleanup.state = CleanupState::Succeeded;
                            job.detail.summary.phase = "recovery_complete".to_string();
                        }
                        if state.persisted.active_job_id.as_deref() == Some(job_id.as_str()) {
                            state.persisted.active_job_id = None;
                        }
                        state.recovering.remove(&job_id);
                        state.recovery_errors.remove(&job_id);
                        if let Err(error) = self.store.save(&state.persisted) {
                            tracing::error!(%job_id, "failed to persist recovery completion: {error}");
                        }
                    }
                    self.emit_best_effort(
                        &job_id,
                        "recovery_complete",
                        "recovery_complete",
                        "interrupted job network and worker resources are clear",
                        None,
                    );
                    return;
                }
                Err(error) => {
                    let message = error.to_string();
                    let changed = {
                        let mut state = self.state.lock().expect("job manager lock");
                        let changed = state.recovery_errors.get(&job_id) != Some(&message);
                        if changed {
                            state
                                .recovery_errors
                                .insert(job_id.clone(), message.clone());
                            if let Some(job) = find_stored_mut(&mut state.persisted, &job_id) {
                                job.detail.summary.cleanup.state = CleanupState::Running;
                                job.detail.summary.phase = "recovering_owned_resources".to_string();
                                if !job.detail.summary.cleanup.errors.contains(&message) {
                                    job.detail.summary.cleanup.errors.push(message.clone());
                                }
                            }
                            if let Err(error) = self.store.save(&state.persisted) {
                                tracing::error!(%job_id, "failed to persist recovery error: {error}");
                            }
                        }
                        changed
                    };
                    if changed {
                        self.emit_best_effort(
                            &job_id,
                            "recovery_pending",
                            "recovering_owned_resources",
                            &message,
                            None,
                        );
                    }
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    }

    fn recover_worker_leases(&self, job_id: &str) -> anyhow::Result<()> {
        let spam = self.spam.status()?;
        for lease in owned_leases(&spam.active_leases, job_id) {
            self.spam.release_lease(
                &lease.lease_id,
                LeaseReleaseRequest {
                    request_id: format!("{}-recovery-release", lease.lease_id),
                    chain_changed: true,
                },
            )?;
        }
        let mining = self.mining.status()?;
        for lease in owned_leases(&mining.active_leases, job_id) {
            self.mining.release_lease(
                &lease.lease_id,
                LeaseReleaseRequest {
                    request_id: format!("{}-recovery-release", lease.lease_id),
                    chain_changed: true,
                },
            )?;
        }
        anyhow::ensure!(
            owned_leases(&self.spam.status()?.active_leases, job_id).is_empty()
                && owned_leases(&self.mining.status()?.active_leases, job_id).is_empty(),
            "worker leases are still present"
        );
        Ok(())
    }

    fn recover_job_resources(self: &Arc<Self>, job_id: &str) -> anyhow::Result<()> {
        let (kind, detail_request, context, faucet_context) = {
            let state = self.state.lock().expect("job manager lock");
            let job = find_stored(&state.persisted, job_id)
                .ok_or_else(|| anyhow::anyhow!("recovery job {job_id} is missing"))?;
            (
                job.detail.summary.kind,
                job.detail.request.clone(),
                job.reorg_recovery.clone(),
                job.faucet_recovery.clone(),
            )
        };
        if kind == JobKind::Faucet {
            return self.recover_faucet_job(job_id, detail_request, faucet_context);
        }
        if faucet_context.is_some() {
            self.recover_faucet_job(job_id, detail_request.clone(), faucet_context)?;
        }
        self.recover_network_resources(job_id)?;
        if context.mutation_may_have_occurred {
            let request = match context.request.clone() {
                Some(request) => request,
                None if kind == JobKind::Reorg => {
                    serde_json::from_value::<ReorgJobRequest>(detail_request)?
                }
                None => anyhow::bail!("interrupted scenario is missing reorg recovery request"),
            };
            let observer = JobReorgObserver {
                manager: self.clone(),
                job_id: job_id.to_string(),
                abort: Arc::new(AtomicBool::new(false)),
                chain_changed: AtomicBool::new(true),
            };
            self.ensure_recovery_leases(job_id)?;
            self.reorg.recover(&request, &context, &observer)?;
        }
        self.recover_worker_leases(job_id)
    }

    fn recover_faucet_job(
        self: &Arc<Self>,
        job_id: &str,
        detail_request: Value,
        context: Option<FaucetRecoveryContext>,
    ) -> anyhow::Result<()> {
        let context =
            context.ok_or_else(|| anyhow::anyhow!("faucet recovery context is missing"))?;
        let request = context
            .normalized_request
            .clone()
            .or_else(|| serde_json::from_value(detail_request).ok())
            .ok_or_else(|| anyhow::anyhow!("normalized faucet request is missing"))?;
        let Some(raw_tx_hex) = context.raw_tx_hex.clone() else {
            if let Some(source) = context.source {
                if !context.selected_inputs.is_empty() {
                    self.faucet
                        .unlock_inputs(source, &context.selected_inputs)?;
                }
            }
            return self.recover_worker_leases(job_id);
        };
        let txid = context
            .txid
            .clone()
            .ok_or_else(|| anyhow::anyhow!("prepared faucet transaction has no txid"))?;

        if let Some(confirmation) = self.faucet.confirmation(&txid)? {
            if self.faucet_store.get(&txid).is_none() {
                let preflight = self.faucet.preflight()?;
                let transfer = self
                    .transfer_from_context(
                        &request,
                        &context,
                        preflight.height,
                        preflight.best_hash,
                        false,
                    )
                    .ok_or_else(|| anyhow::anyhow!("faucet context cannot form a transfer"))?;
                self.faucet_store.arm(StoredFaucetTransfer {
                    public: transfer,
                    raw_tx_hex: Some(raw_tx_hex),
                    selected_inputs: context.selected_inputs.clone(),
                })?;
            }
            self.faucet_store.mark_confirmed(
                &txid,
                confirmation.height,
                confirmation.block_hash,
                now_ms(),
            )?;
            self.mark_recovered_faucet_succeeded(job_id, &txid)?;
            return self.recover_worker_leases(job_id);
        }

        let source = context
            .source
            .ok_or_else(|| anyhow::anyhow!("prepared faucet source is missing"))?;
        if !self
            .faucet
            .inputs_unspent(source, &context.selected_inputs)?
        {
            for node in [FaucetSourceNode::Node2, FaucetSourceNode::Node3] {
                let _ = self.faucet.set_priority(node, &txid, 0);
            }
            let mut state = self.state.lock().expect("job manager lock");
            if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
                job.detail.summary.state = JobState::Failed;
                job.detail.summary.phase = "prepared_inputs_conflicted".to_string();
                job.detail.failure = Some(JobFailure {
                    code: "prepared_inputs_conflicted".to_string(),
                    message: "prepared faucet inputs were spent elsewhere; no replacement was constructed"
                        .to_string(),
                });
            }
            self.store.save(&state.persisted)?;
            drop(state);
            return self.recover_worker_leases(job_id);
        }

        self.ensure_faucet_recovery_mining_lease(job_id)?;
        self.faucet.lock_inputs(source, &context.selected_inputs)?;
        let transaction: bitcoincore_rpc::bitcoin::Transaction =
            bitcoincore_rpc::bitcoin::consensus::encode::deserialize_hex(&raw_tx_hex)?;
        let prepared = PreparedFaucetTransaction {
            raw_tx_hex: raw_tx_hex.clone(),
            txid: txid.clone(),
            input_sats: context
                .input_sats
                .ok_or_else(|| anyhow::anyhow!("prepared faucet input total is missing"))?,
            change_sats: context
                .change_sats
                .ok_or_else(|| anyhow::anyhow!("prepared faucet change is missing"))?,
            vsize: transaction.vsize() as u64,
        };
        self.arm_prepared(job_id, &prepared, &AtomicBool::new(false))?;
        if self.faucet_store.get(&txid).is_none() {
            let preflight = self.faucet.preflight()?;
            let transfer = self
                .transfer_from_context(
                    &request,
                    &self.faucet_context(job_id).unwrap_or(context.clone()),
                    preflight.height,
                    preflight.best_hash,
                    self.faucet
                        .observer_contains_unconfirmed(&txid)
                        .unwrap_or(false),
                )
                .ok_or_else(|| anyhow::anyhow!("faucet context cannot form a transfer"))?;
            self.faucet_store.arm(StoredFaucetTransfer {
                public: transfer,
                raw_tx_hex: Some(raw_tx_hex),
                selected_inputs: context.selected_inputs.clone(),
            })?;
        }
        self.mark_recovered_faucet_succeeded(job_id, &txid)?;
        self.recover_worker_leases(job_id)
    }

    fn ensure_faucet_recovery_mining_lease(&self, job_id: &str) -> anyhow::Result<()> {
        let status = self.mining.status()?;
        if owned_leases(&status.active_leases, job_id).is_empty() {
            let lease_id = format!("{job_id}-mining-1");
            let lease = JobLease {
                component: "mining".to_string(),
                lease_id: lease_id.clone(),
                purpose: "interrupted faucet recovery".to_string(),
            };
            self.persist_lease_intent(job_id, lease.clone())?;
            self.mining.acquire_lease(LeaseRequest {
                lease_id,
                owner_job_id: job_id.to_string(),
                purpose: lease.purpose.clone(),
                ttl_secs: self.lease_ttl_secs,
                request_id: format!("{job_id}-faucet-recovery-acquire-{}", now_ms()),
            })?;
            self.acknowledge_lease(job_id, &lease);
        }
        Ok(())
    }

    fn mark_recovered_faucet_succeeded(&self, job_id: &str, txid: &str) -> anyhow::Result<()> {
        let transfer = self
            .faucet_store
            .get(txid)
            .ok_or_else(|| anyhow::anyhow!("recovered faucet transfer is missing"))?;
        let mut state = self.state.lock().expect("job manager lock");
        if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
            if job.detail.summary.kind == JobKind::Faucet {
                job.detail.summary.state = JobState::Succeeded;
                job.detail.summary.phase = "armed_for_next_block".to_string();
                job.detail.result = Some(serde_json::to_value(transfer)?);
                job.detail.failure = None;
            }
            if let Some(context) = job.faucet_recovery.as_mut() {
                context.raw_tx_hex = None;
                context.selected_inputs.clear();
                context.phase = FaucetPhase::Armed;
            }
        }
        self.store.save(&state.persisted)
    }

    fn recover_network_resources(&self, job_id: &str) -> anyhow::Result<()> {
        let recorded_nodes: HashSet<String> = self
            .get(job_id)
            .map_err(|error| anyhow::anyhow!(error.message))?
            .leases
            .into_iter()
            .filter_map(|lease| network_lease_node(&lease).ok().map(str::to_string))
            .collect();
        let mut affected = recorded_nodes;
        for node in ["node1", "node2", "node3"] {
            let status = self.network.status(node)?;
            if let Some(lease) = status
                .active_lease
                .filter(|lease| lease.owner_job_id == job_id)
            {
                self.network.release_lease(
                    node,
                    &lease.lease_id,
                    NetworkLeaseReleaseRequest {
                        request_id: format!("{}-recovery-release", lease.lease_id),
                    },
                )?;
                affected.insert(node.to_string());
            }
        }
        for node in &affected {
            if node == "node1" {
                continue;
            }
            self.network_actions
                .reconnect_target(parse_miner_node(node).map_err(|error| {
                    anyhow::anyhow!("invalid recovery network node: {}", error.message)
                })?)?;
        }
        if !affected.is_empty() {
            self.network_actions
                .wait_for_convergence(None, &NeverAbortControl)?;
        }
        Ok(())
    }

    fn ensure_recovery_leases(&self, job_id: &str) -> anyhow::Result<()> {
        let nonce = now_ms();
        let spam_status = self.spam.status()?;
        let spam_leases = owned_leases(&spam_status.active_leases, job_id);
        if spam_leases.is_empty() {
            let lease_id = format!("{job_id}-spam");
            let lease = JobLease {
                component: "spam".to_string(),
                lease_id: lease_id.clone(),
                purpose: "interrupted reorg recovery".to_string(),
            };
            self.persist_lease_intent(job_id, lease.clone())?;
            self.spam.acquire_lease(LeaseRequest {
                lease_id: lease_id.clone(),
                owner_job_id: job_id.to_string(),
                purpose: "interrupted reorg recovery".to_string(),
                ttl_secs: self.lease_ttl_secs,
                request_id: format!("{job_id}-spam-recovery-acquire-{nonce}"),
            })?;
            self.acknowledge_lease(job_id, &lease);
        } else {
            for lease in spam_leases {
                self.spam.renew_lease(
                    &lease.lease_id,
                    LeaseRenewRequest {
                        ttl_secs: self.lease_ttl_secs,
                        request_id: format!("{}-recovery-renew-{nonce}", lease.lease_id),
                    },
                )?;
            }
        }

        let mining_status = self.mining.status()?;
        let mining_leases = owned_leases(&mining_status.active_leases, job_id);
        if mining_leases.is_empty() {
            let lease_id = format!("{job_id}-mining");
            let lease = JobLease {
                component: "mining".to_string(),
                lease_id: lease_id.clone(),
                purpose: "interrupted reorg recovery".to_string(),
            };
            self.persist_lease_intent(job_id, lease.clone())?;
            self.mining.acquire_lease(LeaseRequest {
                lease_id: lease_id.clone(),
                owner_job_id: job_id.to_string(),
                purpose: "interrupted reorg recovery".to_string(),
                ttl_secs: self.lease_ttl_secs,
                request_id: format!("{job_id}-mining-recovery-acquire-{nonce}"),
            })?;
            self.acknowledge_lease(job_id, &lease);
        } else {
            for lease in mining_leases {
                self.mining.renew_lease(
                    &lease.lease_id,
                    LeaseRenewRequest {
                        ttl_secs: self.lease_ttl_secs,
                        request_id: format!("{}-recovery-renew-{nonce}", lease.lease_id),
                    },
                )?;
            }
        }
        Ok(())
    }

    fn record_reorg_recovery(&self, job_id: &str, progress: &ReorgProgress) {
        if progress.phase != ReorgPhase::Invalidating {
            return;
        }
        let hash = progress
            .data
            .as_ref()
            .and_then(|data| data.get("hash"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut state = self.state.lock().expect("job manager lock");
        if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
            job.reorg_recovery.mutation_may_have_occurred = true;
            job.reorg_recovery.invalidated_block_hash = hash;
            if let Err(error) = self.store.save(&state.persisted) {
                tracing::error!(job_id, "failed to persist reorg recovery context: {error}");
            }
        }
    }

    fn prepare_scenario_reorg(
        &self,
        job_id: &str,
        request: &ReorgJobRequest,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().expect("job manager lock");
        let job = find_stored_mut(&mut state.persisted, job_id)
            .ok_or_else(|| anyhow::anyhow!("scenario job disappeared"))?;
        job.reorg_recovery = ReorgRecoveryContext {
            mutation_may_have_occurred: true,
            request: Some(request.clone()),
            invalidated_block_hash: None,
        };
        self.store.save(&state.persisted)
    }

    fn mark_scenario_reorg_safe(&self, job_id: &str) {
        let mut state = self.state.lock().expect("job manager lock");
        if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
            job.reorg_recovery = ReorgRecoveryContext::default();
            if let Err(error) = self.store.save(&state.persisted) {
                tracing::error!(job_id, "failed to persist safe reorg boundary: {error}");
            }
        }
    }

    fn record_scenario_progress(&self, job_id: &str, progress: &ScenarioProgress) {
        let phase = progress.phase.as_str();
        let mut state = self.state.lock().expect("job manager lock");
        if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
            if job.detail.summary.state != JobState::AbortRequested {
                job.detail.summary.state = JobState::Running;
            }
            job.detail.summary.phase = phase.to_string();
            job.detail.current_step = Some(ScenarioStepStatus {
                index: progress.step_index,
                total: progress.total_steps,
                kind: progress.step_kind.clone(),
                state: match progress.phase {
                    ScenarioProgressPhase::StepStarted => "running",
                    ScenarioProgressPhase::StepCompleted => "completed",
                    ScenarioProgressPhase::StepFailed => "failed",
                    ScenarioProgressPhase::AbortObserved => "aborted",
                }
                .to_string(),
            });
            if let Err(error) = self.store.save(&state.persisted) {
                tracing::error!(job_id, "failed to persist scenario progress: {error}");
            }
        }
        drop(state);
        self.emit_best_effort(
            job_id,
            "scenario_progress",
            phase,
            &progress.message,
            serde_json::to_value(progress).ok(),
        );
    }

    fn reach_scenario_checkpoint(
        &self,
        job_id: &str,
        checkpoint: &CheckpointStep,
        step_index: usize,
    ) -> anyhow::Result<Value> {
        // A checkpoint is not externally observable until its full live
        // summary and generation are durably recorded together.
        let live_summary = self.scenario.live_summary()?;
        let generation = {
            let mut state = self.state.lock().expect("job manager lock");
            let generation = state.persisted.next_checkpoint_generation.max(1);
            state.persisted.next_checkpoint_generation = generation.saturating_add(1);
            let job = find_stored_mut(&mut state.persisted, job_id)
                .ok_or_else(|| anyhow::anyhow!("scenario job disappeared"))?;
            let stored = job
                .detail
                .checkpoints
                .iter_mut()
                .find(|stored| stored.name == checkpoint.name)
                .ok_or_else(|| anyhow::anyhow!("scenario checkpoint disappeared"))?;
            stored.generation = generation;
            stored.state = CheckpointState::Reached;
            stored.arrived_at_ms = Some(now_ms());
            stored.live_summary = Some(live_summary.clone());
            job.detail.summary.phase = if checkpoint.pause {
                "waiting_at_checkpoint"
            } else {
                "checkpoint_reached"
            }
            .to_string();
            if checkpoint.pause && job.detail.summary.state != JobState::AbortRequested {
                job.detail.summary.state = JobState::WaitingAtCheckpoint;
            }
            job.detail.current_step = Some(ScenarioStepStatus {
                index: step_index,
                total: job
                    .detail
                    .current_step
                    .as_ref()
                    .map(|step| step.total)
                    .unwrap_or(step_index),
                kind: "checkpoint".to_string(),
                state: if checkpoint.pause {
                    "waiting"
                } else {
                    "reached"
                }
                .to_string(),
            });
            self.store.save(&state.persisted)?;
            generation
        };

        self.emit_best_effort(
            job_id,
            "checkpoint_reached",
            if checkpoint.pause {
                "waiting_at_checkpoint"
            } else {
                "checkpoint_reached"
            },
            &format!("checkpoint '{}' reached", checkpoint.name),
            Some(json!({
                "name": checkpoint.name,
                "generation": generation,
                "pause": checkpoint.pause,
                "live_summary": live_summary
            })),
        );
        self.checkpoint_cv.notify_all();

        if !checkpoint.pause {
            return Ok(json!({
                "checkpoint": checkpoint.name,
                "generation": generation,
                "pause": false
            }));
        }

        let timeout = Duration::from_secs(
            checkpoint
                .timeout_secs
                .expect("validated pausing checkpoint timeout"),
        );
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().expect("job manager lock");
        loop {
            let job = find_stored(&state.persisted, job_id)
                .ok_or_else(|| anyhow::anyhow!("scenario job disappeared"))?;
            let stored = job
                .detail
                .checkpoints
                .iter()
                .find(|stored| stored.name == checkpoint.name)
                .ok_or_else(|| anyhow::anyhow!("scenario checkpoint disappeared"))?;
            if stored.generation != generation {
                anyhow::bail!("checkpoint generation changed while waiting");
            }
            if stored.state == CheckpointState::Released {
                return Ok(json!({
                    "checkpoint": checkpoint.name,
                    "generation": generation,
                    "released": true
                }));
            }
            if state
                .aborts
                .get(job_id)
                .is_some_and(|abort| abort.load(Ordering::Acquire))
            {
                return Ok(json!({
                    "checkpoint": checkpoint.name,
                    "generation": generation,
                    "aborted": true
                }));
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let job = find_stored_mut(&mut state.persisted, job_id)
                    .expect("scenario job checked above");
                let stored = job
                    .detail
                    .checkpoints
                    .iter_mut()
                    .find(|stored| stored.name == checkpoint.name)
                    .expect("checkpoint checked above");
                stored.state = CheckpointState::TimedOut;
                job.detail.summary.state = JobState::Running;
                job.detail.summary.phase = "checkpoint_timed_out".to_string();
                if let Some(step) = job.detail.current_step.as_mut() {
                    step.state = "timed_out".to_string();
                }
                self.store.save(&state.persisted)?;
                drop(state);
                self.emit_best_effort(
                    job_id,
                    "checkpoint_timed_out",
                    "checkpoint_timed_out",
                    &format!("checkpoint '{}' timed out", checkpoint.name),
                    Some(json!({
                        "name": checkpoint.name,
                        "generation": generation,
                        "timeout_secs": checkpoint.timeout_secs
                    })),
                );
                anyhow::bail!(
                    "checkpoint '{}' timed out after {} seconds",
                    checkpoint.name,
                    timeout.as_secs()
                );
            }
            let (next_state, _) = self
                .checkpoint_cv
                .wait_timeout(state, remaining)
                .expect("job manager checkpoint wait");
            state = next_state;
        }
    }
}

struct SimpleJobControl {
    abort: Arc<AtomicBool>,
}

struct NeverAbortControl;

impl ScenarioControl for NeverAbortControl {
    fn observe(&self, _progress: ScenarioProgress) {}

    fn abort_requested(&self) -> bool {
        false
    }
}

impl ScenarioControl for SimpleJobControl {
    fn observe(&self, _progress: ScenarioProgress) {}

    fn abort_requested(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct ScenarioRuntime {
    next_lease_sequence: u64,
    mining_lease: Option<JobLease>,
    spam_lease: Option<JobLease>,
    network_leases: Vec<JobLease>,
    mining_paused_by_step: bool,
    chain_changed: bool,
}

struct JobScenarioActions {
    manager: Arc<JobManager>,
    job_id: String,
    abort: Arc<AtomicBool>,
    use_raw_tx_spam: bool,
    runtime: Arc<Mutex<ScenarioRuntime>>,
}

impl JobScenarioActions {
    fn apply_context(&self) -> ApplyContext<'_> {
        ApplyContext {
            apply_lock: self.manager.apply_lock.as_ref(),
            control_store: &self.manager.control_store,
            control_state: self.manager.control_state.as_ref(),
            chain: self.manager.chain.as_ref(),
            mining: self.manager.mining.as_ref(),
            spam: self.manager.spam.as_ref(),
        }
    }

    fn acquire(&self, component: &str, purpose: &str) -> anyhow::Result<JobLease> {
        let sequence = {
            let mut runtime = self.runtime.lock().expect("scenario runtime lock");
            runtime.next_lease_sequence = runtime.next_lease_sequence.saturating_add(1);
            runtime.next_lease_sequence
        };
        self.manager
            .acquire_scenario_lease(&self.job_id, component, purpose, sequence)
    }

    fn ensure_spam_lease(&self, purpose: &str) -> anyhow::Result<bool> {
        if self
            .runtime
            .lock()
            .expect("scenario runtime lock")
            .spam_lease
            .is_some()
        {
            return Ok(false);
        }
        let lease = self.acquire("spam", purpose)?;
        self.runtime
            .lock()
            .expect("scenario runtime lock")
            .spam_lease = Some(lease);
        Ok(true)
    }

    fn ensure_mining_lease(&self, purpose: &str) -> anyhow::Result<bool> {
        if self
            .runtime
            .lock()
            .expect("scenario runtime lock")
            .mining_lease
            .is_some()
        {
            return Ok(false);
        }
        let lease = self.acquire("mining", purpose)?;
        self.runtime
            .lock()
            .expect("scenario runtime lock")
            .mining_lease = Some(lease);
        Ok(true)
    }

    fn release_component(&self, component: &str) -> anyhow::Result<bool> {
        let (lease, chain_changed) = {
            let mut runtime = self.runtime.lock().expect("scenario runtime lock");
            let lease = match component {
                "spam" => runtime.spam_lease.take(),
                "mining" => runtime.mining_lease.take(),
                other => anyhow::bail!("unknown lease component {other}"),
            };
            (lease, runtime.chain_changed)
        };
        let Some(lease) = lease else {
            return Ok(false);
        };
        if let Err(error) = self
            .manager
            .release_scenario_lease(&self.job_id, &lease, chain_changed)
        {
            let mut runtime = self.runtime.lock().expect("scenario runtime lock");
            match component {
                "spam" => runtime.spam_lease = Some(lease),
                "mining" => runtime.mining_lease = Some(lease),
                _ => unreachable!("component validated above"),
            }
            return Err(error);
        }
        Ok(true)
    }

    fn remember_network_lease(&self, lease: JobLease) {
        let mut runtime = self.runtime.lock().expect("scenario runtime lock");
        if !runtime
            .network_leases
            .iter()
            .any(|existing| existing.lease_id == lease.lease_id)
        {
            runtime.network_leases.push(lease);
        }
    }

    fn forget_network_lease(&self, lease_id: &str) {
        let mut runtime = self.runtime.lock().expect("scenario runtime lock");
        runtime
            .network_leases
            .retain(|lease| lease.lease_id != lease_id);
    }

    fn resolve_faucet_outputs(
        &self,
        outputs: &[FaucetScenarioOutput],
    ) -> anyhow::Result<Vec<FaucetOutput>> {
        outputs
            .iter()
            .map(|output| {
                let address = match (&output.address, &output.address_env) {
                    (Some(address), None) => address.trim().to_string(),
                    (None, Some(env)) => std::env::var(env)
                        .map(|value| value.trim().to_string())
                        .map_err(|_| {
                        anyhow::anyhow!("environment variable {env} is not set for faucet address")
                    })?,
                    _ => anyhow::bail!("exactly one of address or address_env is required"),
                };
                anyhow::ensure!(!address.is_empty(), "faucet address must not be empty");
                Ok(FaucetOutput {
                    address,
                    amount_sats: output.amount_sats()?,
                })
            })
            .collect()
    }

    fn resolve_wait_txid(&self, wait: &WaitTxStep) -> anyhow::Result<String> {
        let txid = match (&wait.txid, &wait.txid_env) {
            (Some(txid), None) => txid.trim().to_string(),
            (None, Some(env)) => std::env::var(env)
                .map(|value| value.trim().to_string())
                .map_err(|_| {
                    anyhow::anyhow!("environment variable {env} is not set for wait_tx txid")
                })?,
            _ => anyhow::bail!("exactly one of txid or txid_env is required"),
        };
        anyhow::ensure!(!txid.is_empty(), "wait_tx txid must not be empty");
        Ok(txid)
    }

    fn wait_faucet_confirmation(
        &self,
        txid: &str,
        timeout_secs: u64,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if control.abort_requested() {
                return Ok(json!({"txid": txid, "aborted": true}));
            }
            if let Some(transfer) = self.manager.faucet_transfer(txid) {
                match transfer.delivery_state {
                    FaucetDeliveryState::Confirmed => {
                        return serde_json::to_value(transfer).map_err(Into::into);
                    }
                    FaucetDeliveryState::DeliveryFailed
                    | FaucetDeliveryState::AbortedAfterSubmission
                    | FaucetDeliveryState::OrphanedAfterConfirmation => {
                        anyhow::bail!(
                            "faucet transfer {txid} ended {}{}",
                            transfer.delivery_state.as_str(),
                            transfer
                                .last_error
                                .as_deref()
                                .map(|error| format!(": {error}"))
                                .unwrap_or_default()
                        );
                    }
                    FaucetDeliveryState::Armed | FaucetDeliveryState::Recovering => {}
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for faucet transfer {txid} to confirm");
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    fn configure_faucet_recovery(&self, request: &FaucetJobRequest) -> anyhow::Result<()> {
        let mut state = self.manager.state.lock().expect("job manager lock");
        let job = find_stored_mut(&mut state.persisted, &self.job_id)
            .ok_or_else(|| anyhow::anyhow!("scenario job disappeared"))?;
        job.faucet_recovery = Some(FaucetRecoveryContext {
            phase: FaucetPhase::Validated,
            normalized_request: Some(request.clone()),
            desired_priority_delta_sats: FAUCET_PRIORITY_DELTA_SATS,
            ..FaucetRecoveryContext::default()
        });
        self.manager.store.save(&state.persisted)?;
        Ok(())
    }

    fn assert_effective_config(
        &self,
        desired_generation: u64,
        expected: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let mining = self.manager.mining.status()?;
        let spam = self.manager.spam.status()?;
        let mining_values = mining
            .policy
            .canonical_values()
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<BTreeMap<_, _>>();
        let spam_values = spam
            .policy
            .canonical_values()
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<BTreeMap<_, _>>();
        let mut mismatches = Vec::new();
        for (key, expected_value) in expected {
            let spec = live_tuning::spec(key.as_str())
                .ok_or_else(|| anyhow::anyhow!("unknown runtime setting {key}"))?;
            let (component, generation, values) = match spec.scope {
                ServiceScope::MiningController => {
                    ("mining", mining.effective_generation, &mining_values)
                }
                ServiceScope::Spammer => ("spam", spam.effective_generation, &spam_values),
            };
            if generation != desired_generation {
                mismatches.push(format!(
                    "{component} generation {generation}, expected {desired_generation}"
                ));
            }
            match values.get(key) {
                Some(actual) if actual == expected_value => {}
                Some(actual) => mismatches.push(format!(
                    "{component}.{key}={actual}, expected {expected_value}"
                )),
                None => mismatches.push(format!("{component}.{key} is not exposed")),
            }
        }
        anyhow::ensure!(
            mismatches.is_empty(),
            "effective config mismatch: {}",
            mismatches.join("; ")
        );
        Ok(())
    }

    fn current_height(&self) -> anyhow::Result<u64> {
        let summary = self.manager.scenario.live_summary()?;
        summary
            .get("height")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("live summary did not include a numeric height"))
    }

    fn current_mempool_tx_count(&self) -> anyhow::Result<usize> {
        let summary = self.manager.scenario.live_summary()?;
        let count = summary
            .get("mempool")
            .and_then(|mempool| mempool.get("transactions"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                anyhow::anyhow!("live summary did not include a numeric mempool transaction count")
            })?;
        count
            .try_into()
            .map_err(|_| anyhow::anyhow!("mempool transaction count is out of range"))
    }

    fn unreachable_component(error: impl ToString) -> ComponentState {
        ComponentState {
            reachable: false,
            status: "unreachable".to_string(),
            last_error: Some(error.to_string()),
            ..ComponentState::default()
        }
    }

    fn component_snapshot(&self, component: ScenarioComponent) -> ComponentState {
        match component {
            ScenarioComponent::Mining => match self.manager.mining.status() {
                Ok(status) => ComponentState {
                    reachable: true,
                    status: status.phase.as_str().to_string(),
                    phase: Some(status.phase.as_str().to_string()),
                    effective_generation: Some(status.effective_generation),
                    uptime_secs: Some(status.uptime_secs),
                    last_error: status.last_error,
                    desired_state: Some(status.desired_state),
                    effective_state: Some(status.effective_state),
                    observed_height: status.height,
                    next_scheduled_attempt_ms: status.next_scheduled_attempt_ms,
                    last_mined_block: status.last_mined_block,
                    active_lease_count: Some(status.active_leases.len()),
                    ..ComponentState::default()
                },
                Err(error) => Self::unreachable_component(error),
            },
            ScenarioComponent::Spam => match self.manager.spam.status() {
                Ok(status) => ComponentState {
                    reachable: true,
                    status: status.phase.as_str().to_string(),
                    phase: Some(status.phase.as_str().to_string()),
                    effective_generation: Some(status.effective_generation),
                    uptime_secs: Some(status.uptime_secs),
                    last_error: status.last_error,
                    desired_state: Some(status.desired_state),
                    effective_state: Some(status.effective_state),
                    observed_height: status.observed_height,
                    active_lease_count: Some(status.active_leases.len()),
                    cycle_phase: status.cycle_phase,
                    accepted_transactions: Some(status.accepted_transactions),
                    last_cycle_duration_ms: status.last_cycle_duration_ms,
                    reconciliation_pending: Some(status.reconciliation_pending),
                    ..ComponentState::default()
                },
                Err(error) => Self::unreachable_component(error),
            },
            ScenarioComponent::NetworkAgentNode1
            | ScenarioComponent::NetworkAgentNode2
            | ScenarioComponent::NetworkAgentNode3 => {
                let node = component
                    .network_node()
                    .expect("network component has a node");
                match self.manager.network.status(node) {
                    Ok(status) => {
                        let impaired = status.active_lease.is_some();
                        ComponentState {
                            reachable: true,
                            status: if impaired { "impaired" } else { "clear" }.to_string(),
                            phase: Some(if impaired { "active" } else { "clear" }.to_string()),
                            effective_generation: Some(status.effective_generation),
                            uptime_secs: Some(status.uptime_secs),
                            last_error: status.last_error,
                            active_lease_count: Some(usize::from(impaired)),
                            ..ComponentState::default()
                        }
                    }
                    Err(error) => Self::unreachable_component(error),
                }
            }
        }
    }

    fn assert_component_expectation(
        &self,
        expected: &ComponentExpectation,
    ) -> anyhow::Result<ComponentState> {
        let actual = self.component_snapshot(expected.component);
        let mut mismatches = Vec::new();

        if let Some(expected_reachable) = expected.reachable {
            if actual.reachable != expected_reachable {
                mismatches.push(format!(
                    "reachable={}, expected {expected_reachable}",
                    actual.reachable
                ));
            }
        }
        if let Some(expected_status) = expected.status.as_deref() {
            if actual.status != expected_status {
                mismatches.push(format!(
                    "status={}, expected {expected_status}",
                    actual.status
                ));
            }
        }
        if let Some(expected_phase) = expected.phase.as_deref() {
            match actual.phase.as_deref() {
                Some(actual_phase) if actual_phase == expected_phase => {}
                Some(actual_phase) => {
                    mismatches.push(format!("phase={actual_phase}, expected {expected_phase}"));
                }
                None => mismatches.push(format!("phase is not exposed, expected {expected_phase}")),
            }
        }
        if let Some(expected_state) = expected.desired_state {
            match actual.desired_state {
                Some(actual_state) if actual_state == expected_state => {}
                Some(actual_state) => mismatches.push(format!(
                    "desired_state={}, expected {}",
                    actual_state.as_str(),
                    expected_state.as_str()
                )),
                None => mismatches.push(format!(
                    "desired_state is not exposed, expected {}",
                    expected_state.as_str()
                )),
            }
        }
        if let Some(expected_state) = expected.effective_state {
            match actual.effective_state {
                Some(actual_state) if actual_state == expected_state => {}
                Some(actual_state) => mismatches.push(format!(
                    "effective_state={}, expected {}",
                    actual_state.as_str(),
                    expected_state.as_str()
                )),
                None => mismatches.push(format!(
                    "effective_state is not exposed, expected {}",
                    expected_state.as_str()
                )),
            }
        }
        if let Some(expected_generation) = expected.effective_generation {
            match actual.effective_generation {
                Some(actual_generation) if actual_generation == expected_generation => {}
                Some(actual_generation) => mismatches.push(format!(
                    "effective_generation={actual_generation}, expected {expected_generation}"
                )),
                None => mismatches.push(format!(
                    "effective_generation is not exposed, expected {expected_generation}"
                )),
            }
        }
        if let Some(min_height) = expected.observed_height_at_least {
            match actual.observed_height {
                Some(actual_height) if actual_height >= min_height => {}
                Some(actual_height) => mismatches.push(format!(
                    "observed_height={actual_height}, expected at least {min_height}"
                )),
                None => mismatches.push(format!(
                    "observed_height is not exposed, expected at least {min_height}"
                )),
            }
        }
        if let Some(expected_count) = expected.active_lease_count {
            match actual.active_lease_count {
                Some(actual_count) if actual_count == expected_count => {}
                Some(actual_count) => mismatches.push(format!(
                    "active_lease_count={actual_count}, expected {expected_count}"
                )),
                None => mismatches.push(format!(
                    "active_lease_count is not exposed, expected {expected_count}"
                )),
            }
        }
        if let Some(expected_phase) = expected.cycle_phase.as_deref() {
            match actual.cycle_phase.as_deref() {
                Some(actual_phase) if actual_phase == expected_phase => {}
                Some(actual_phase) => mismatches.push(format!(
                    "cycle_phase={actual_phase}, expected {expected_phase}"
                )),
                None => mismatches.push(format!(
                    "cycle_phase is not exposed, expected {expected_phase}"
                )),
            }
        }

        anyhow::ensure!(
            mismatches.is_empty(),
            "component {} mismatch: {}",
            expected.component,
            mismatches.join("; ")
        );
        Ok(actual)
    }

    fn assert_height_bounds(
        &self,
        equals: Option<u64>,
        at_least: Option<u64>,
        at_most: Option<u64>,
    ) -> anyhow::Result<Value> {
        let height = self.current_height()?;
        let mut mismatches = Vec::new();
        if let Some(expected) = equals {
            if height != expected {
                mismatches.push(format!("height={height}, expected {expected}"));
            }
        }
        if let Some(minimum) = at_least {
            if height < minimum {
                mismatches.push(format!("height={height}, expected at least {minimum}"));
            }
        }
        if let Some(maximum) = at_most {
            if height > maximum {
                mismatches.push(format!("height={height}, expected at most {maximum}"));
            }
        }
        anyhow::ensure!(
            mismatches.is_empty(),
            "height assertion failed: {}",
            mismatches.join("; ")
        );
        Ok(json!({
            "height": height,
            "equals": equals,
            "at_least": at_least,
            "at_most": at_most
        }))
    }

    fn check_wait_condition(&self, condition: &WaitCondition) -> anyhow::Result<Value> {
        match condition {
            WaitCondition::HeightAtLeast { height } => {
                self.assert_height_bounds(None, Some(*height), None)
            }
            WaitCondition::MempoolTxsAtLeast { count } => {
                let actual = self.current_mempool_tx_count()?;
                anyhow::ensure!(
                    actual >= *count,
                    "mempool tx count {actual}, expected at least {count}"
                );
                Ok(json!({"mempool_txs": actual, "at_least": count}))
            }
            WaitCondition::MempoolTxsAtMost { count } => {
                let actual = self.current_mempool_tx_count()?;
                anyhow::ensure!(
                    actual <= *count,
                    "mempool tx count {actual}, expected at most {count}"
                );
                Ok(json!({"mempool_txs": actual, "at_most": count}))
            }
            WaitCondition::Component { expected } => {
                serde_json::to_value(self.assert_component_expectation(expected)?)
                    .map_err(Into::into)
            }
        }
    }
}

impl ScenarioControl for JobScenarioActions {
    fn observe(&self, progress: ScenarioProgress) {
        self.manager
            .record_scenario_progress(&self.job_id, &progress);
    }

    fn abort_requested(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }
}

impl ScenarioActions for JobScenarioActions {
    fn wait_height(&self, height: u64, control: &dyn ScenarioControl) -> anyhow::Result<Value> {
        self.manager.scenario.wait_height(height, control)
    }

    fn set_mining_paused(&self, paused: bool) -> anyhow::Result<Value> {
        if paused {
            let acquired = self.ensure_mining_lease("scenario pause_mining step")?;
            self.runtime
                .lock()
                .expect("scenario runtime lock")
                .mining_paused_by_step = true;
            Ok(json!({"paused": true, "lease_acquired": acquired}))
        } else {
            let released = self.release_component("mining")?;
            self.runtime
                .lock()
                .expect("scenario runtime lock")
                .mining_paused_by_step = false;
            Ok(json!({"paused": false, "lease_released": released}))
        }
    }

    fn mine(&self, node: MinerNode, blocks: u64) -> anyhow::Result<Value> {
        self.manager.scenario.mine(node, blocks)
    }

    fn run_reorg(
        &self,
        depth: u64,
        empty: bool,
        node: MinerNode,
        adds_new_txs: u64,
        double_spend_pct: u8,
        _control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        let acquired_spam = self.ensure_spam_lease("scenario reorg step")?;
        let acquired_mining = self.ensure_mining_lease("scenario reorg step")?;
        let request = ReorgJobRequest {
            depth,
            empty,
            node: node.short_name().to_string(),
            adds_new_txs,
            double_spend_pct,
        };
        self.manager
            .prepare_scenario_reorg(&self.job_id, &request)?;
        let observer = JobReorgObserver {
            manager: self.manager.clone(),
            job_id: self.job_id.clone(),
            abort: self.abort.clone(),
            chain_changed: AtomicBool::new(false),
        };
        let execution = self
            .manager
            .reorg
            .execute(&request, self.use_raw_tx_spam, &observer);
        let chain_changed = execution
            .as_ref()
            .map(|execution| execution.chain_changed)
            .unwrap_or(false)
            || observer.chain_changed.load(Ordering::Acquire);
        self.runtime
            .lock()
            .expect("scenario runtime lock")
            .chain_changed |= chain_changed;
        // The production executor does not return after a possible history
        // mutation until its strict witness is safe, including error paths.
        self.manager.mark_scenario_reorg_safe(&self.job_id);

        let mut cleanup_errors = Vec::new();
        if acquired_spam {
            if let Err(error) = self.release_component("spam") {
                cleanup_errors.push(format!("spam lease: {error}"));
            }
        }
        let mining_paused_by_step = self
            .runtime
            .lock()
            .expect("scenario runtime lock")
            .mining_paused_by_step;
        if acquired_mining && !mining_paused_by_step {
            if let Err(error) = self.release_component("mining") {
                cleanup_errors.push(format!("mining lease: {error}"));
            }
        }

        let execution = execution?;
        anyhow::ensure!(
            cleanup_errors.is_empty(),
            "reorg completed but lease cleanup failed: {}",
            cleanup_errors.join("; ")
        );
        Ok(execution.result)
    }

    fn assert_height(
        &self,
        equals: Option<u64>,
        at_least: Option<u64>,
        at_most: Option<u64>,
    ) -> anyhow::Result<Value> {
        self.assert_height_bounds(equals, at_least, at_most)
    }

    fn assert_component(&self, expected: &ComponentExpectation) -> anyhow::Result<Value> {
        serde_json::to_value(self.assert_component_expectation(expected)?).map_err(Into::into)
    }

    fn wait_until(
        &self,
        condition: &WaitCondition,
        timeout_secs: u64,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if control.abort_requested() {
                return Ok(json!({
                    "condition": condition.kind(),
                    "aborted": true
                }));
            }
            let last_error = match self.check_wait_condition(condition) {
                Ok(value) => {
                    return Ok(json!({
                        "condition": condition.kind(),
                        "satisfied": true,
                        "data": value
                    }));
                }
                Err(error) => format!("{error:#}"),
            };
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out after {timeout_secs}s waiting for condition {}; last observed: {last_error}",
                    condition.kind(),
                );
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    fn wait_tx(&self, wait: &WaitTxStep, control: &dyn ScenarioControl) -> anyhow::Result<Value> {
        let txid = self.resolve_wait_txid(wait)?;
        self.manager.scenario.wait_tx(
            &txid,
            wait.state,
            wait.expected_confirmations(),
            wait.timeout_secs,
            control,
        )
    }

    fn spam_burst(
        &self,
        node: MinerNode,
        txs: u64,
        outputs_per_tx: u64,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        self.manager
            .scenario
            .spam_burst(node, txs, outputs_per_tx, control)
    }

    fn set_config(&self, settings: &BTreeMap<String, String>) -> anyhow::Result<Value> {
        let context = self.apply_context();
        let report = apply_with_context(
            &context,
            ApplyRequest {
                settings: settings.clone(),
                base_generation: None,
            },
            |request| {
                if request.settings.contains_key("FALLBACK_FEE")
                    && self.manager.has_pending_faucet()
                {
                    return Err(simchain_common::control_api::ApiError::new(
                        ErrorCode::FaucetDeliveryPending,
                        "FALLBACK_FEE cannot change while a faucet transfer is armed",
                    ));
                }
                Ok(())
            },
        )
        .map_err(|error| anyhow::anyhow!("config apply failed: {}", error.message))?;
        serde_json::to_value(report).map_err(Into::into)
    }

    fn assert_config(
        &self,
        settings: &BTreeMap<String, String>,
        effective: bool,
    ) -> anyhow::Result<Value> {
        let control = self.manager.control_store.load_current()?;
        let mut mismatches = Vec::new();
        for (key, expected) in settings {
            match control.desired.get(key) {
                Some(actual) if actual == expected => {}
                Some(actual) => {
                    mismatches.push(format!("desired.{key}={actual}, expected {expected}"));
                }
                None => mismatches.push(format!("desired.{key} is not set")),
            }
        }
        anyhow::ensure!(
            mismatches.is_empty(),
            "desired config mismatch: {}",
            mismatches.join("; ")
        );
        if effective {
            self.assert_effective_config(control.generation, settings)?;
        }
        Ok(json!({
            "generation": control.generation,
            "settings": settings,
            "effective": effective
        }))
    }

    fn faucet(
        &self,
        source: FaucetSource,
        outputs: &[FaucetScenarioOutput],
        wait_confirmed: bool,
        timeout_secs: u64,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(
            self.manager.faucet_store.pending().is_none(),
            "a faucet transfer is already armed and awaiting confirmation"
        );
        let request = FaucetJobRequest {
            source,
            outputs: self.resolve_faucet_outputs(outputs)?,
        };
        let request =
            normalize_faucet_request(request, self.manager.faucet_settings.max_request_sats)
                .map_err(|error| anyhow::anyhow!(error.message))?;
        self.configure_faucet_recovery(&request)?;
        self.manager.set_phase(&self.job_id, "faucet_preflight");
        let initial = self.manager.faucet.preflight()?;
        self.manager.emit_best_effort(
            &self.job_id,
            "faucet_preflight_completed",
            "faucet_preflight",
            "faucet wallets, miners, and common tip are ready",
            Some(json!({"height": initial.height, "best_hash": initial.best_hash})),
        );

        let acquired_mining = self.ensure_mining_lease("scenario faucet step")?;
        let outcome = self
            .manager
            .execute_faucet(&self.job_id, &request, self.abort.as_ref());
        let context = self
            .manager
            .faucet_context(&self.job_id)
            .unwrap_or_default();
        if outcome.is_err() || control.abort_requested() {
            if let Some(txid) = context.txid.as_deref() {
                for node in [FaucetSourceNode::Node2, FaucetSourceNode::Node3] {
                    let _ = self.manager.faucet.set_priority(node, txid, 0);
                }
            }
            if let Some(source) = context.source {
                if !context.selected_inputs.is_empty() {
                    let _ = self
                        .manager
                        .faucet
                        .unlock_inputs(source, &context.selected_inputs);
                }
            }
        }

        let mining_paused_by_step = self
            .runtime
            .lock()
            .expect("scenario runtime lock")
            .mining_paused_by_step;
        if acquired_mining && !mining_paused_by_step {
            self.release_component("mining")?;
        }

        let transfer = outcome.map_err(anyhow::Error::from)?;
        self.manager
            .clear_faucet_job_recovery_material(&self.job_id);
        if !wait_confirmed {
            return serde_json::to_value(transfer).map_err(Into::into);
        }
        self.wait_faucet_confirmation(&transfer.txid, timeout_secs, control)
    }

    fn run_partition(
        &self,
        node: MinerNode,
        main_blocks: u64,
        isolated_blocks: u64,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        let acquired_spam = self.ensure_spam_lease("scenario partition step")?;
        let acquired_mining = self.ensure_mining_lease("scenario partition step")?;
        let network = self.manager.acquire_network_lease(
            &self.job_id,
            node.short_name(),
            "scenario partition step",
            NetworkImpairment::Partition {
                ingress_drop: true,
                egress_drop: true,
            },
            {
                let mut runtime = self.runtime.lock().expect("scenario runtime lock");
                runtime.next_lease_sequence = runtime.next_lease_sequence.saturating_add(1);
                runtime.next_lease_sequence
            },
        )?;
        self.remember_network_lease(network.clone());
        let chain_changed = AtomicBool::new(false);
        let execution = self.manager.execute_partition(
            &self.job_id,
            node,
            main_blocks,
            isolated_blocks,
            control,
            &chain_changed,
        );
        self.runtime
            .lock()
            .expect("scenario runtime lock")
            .chain_changed |= chain_changed.load(Ordering::Acquire);

        let mut cleanup_errors = Vec::new();
        if execution.is_ok() {
            // execute_partition healed its owned network lease. Temporary
            // worker leases can now be released in the required spam/mining
            // order. On failure, retain all three for final cleanup, which
            // heals the network before either worker resumes.
            self.forget_network_lease(&network.lease_id);
            if acquired_spam {
                if let Err(error) = self.release_component("spam") {
                    cleanup_errors.push(format!("spam lease: {error}"));
                }
            }
            let mining_paused_by_step = self
                .runtime
                .lock()
                .expect("scenario runtime lock")
                .mining_paused_by_step;
            if acquired_mining && !mining_paused_by_step {
                if let Err(error) = self.release_component("mining") {
                    cleanup_errors.push(format!("mining lease: {error}"));
                }
            }
        }
        let value = execution?;
        anyhow::ensure!(
            cleanup_errors.is_empty(),
            "partition completed but worker lease cleanup failed: {}",
            cleanup_errors.join("; ")
        );
        Ok(value)
    }

    fn degrade(
        &self,
        node: NetworkNode,
        delay_ms: u64,
        loss_pct: f64,
        seconds: Option<u64>,
        until_height: Option<u64>,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        let network = self.manager.acquire_network_lease(
            &self.job_id,
            node.short_name(),
            "scenario timed network degradation",
            NetworkImpairment::Netem { delay_ms, loss_pct },
            {
                let mut runtime = self.runtime.lock().expect("scenario runtime lock");
                runtime.next_lease_sequence = runtime.next_lease_sequence.saturating_add(1);
                runtime.next_lease_sequence
            },
        )?;
        self.remember_network_lease(network.clone());
        self.manager
            .set_phase(&self.job_id, "observing_degraded_network");
        let started = Instant::now();
        let wait_result = if let Some(seconds) = seconds {
            let duration = Duration::from_secs(seconds);
            while started.elapsed() < duration && !control.abort_requested() {
                thread::sleep(
                    Duration::from_millis(100).min(duration.saturating_sub(started.elapsed())),
                );
            }
            Ok(json!({
                "requested_seconds": seconds,
                "aborted": control.abort_requested()
            }))
        } else if let Some(height) = until_height {
            self.manager.scenario.wait_height(height, control)
        } else {
            Err(anyhow::anyhow!(
                "validated scenario degradation duration disappeared"
            ))
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let release_result = self.manager.release_network_lease(&self.job_id, &network);
        if release_result.is_ok() {
            self.forget_network_lease(&network.lease_id);
        }
        let wait_value = match (wait_result, release_result) {
            (Ok(value), Ok(())) => value,
            (Err(wait_error), Ok(())) => return Err(wait_error),
            (Ok(_), Err(release_error)) => return Err(release_error),
            (Err(wait_error), Err(release_error)) => {
                anyhow::bail!("{wait_error:#}; network lease release failed: {release_error:#}");
            }
        };
        Ok(json!({
            "node": node.short_name(),
            "delay_ms": delay_ms,
            "loss_pct": loss_pct,
            "seconds": seconds,
            "until_height": until_height,
            "wait": wait_value,
            "elapsed_ms": elapsed_ms,
            "aborted": control.abort_requested()
        }))
    }

    fn reach_checkpoint(
        &self,
        checkpoint: &CheckpointStep,
        step_index: usize,
        _control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        self.manager
            .reach_scenario_checkpoint(&self.job_id, checkpoint, step_index)
    }

    fn live_summary(&self) -> anyhow::Result<Value> {
        self.manager.scenario.live_summary()
    }
}

struct JobReorgObserver {
    manager: Arc<JobManager>,
    job_id: String,
    abort: Arc<AtomicBool>,
    chain_changed: AtomicBool,
}

impl ReorgObserver for JobReorgObserver {
    fn observe(&self, progress: ReorgProgress) {
        if matches!(
            progress.phase,
            ReorgPhase::Invalidating | ReorgPhase::Invalidated
        ) {
            // Mark conservatively before/around the non-idempotent RPC: a
            // transport error can hide a server-side success.
            self.chain_changed.store(true, Ordering::Release);
        }
        self.manager.record_reorg_recovery(&self.job_id, &progress);
        self.manager.emit_best_effort(
            &self.job_id,
            "reorg_progress",
            progress.phase.as_str(),
            &progress.message,
            progress.data,
        );
    }

    fn abort_requested(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }
}

struct LeaseRenewer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl LeaseRenewer {
    fn start(
        manager: Arc<JobManager>,
        job_id: String,
        abort: Arc<AtomicBool>,
        mining: Arc<dyn MiningControlBackend>,
        spam: Arc<dyn SpamControlBackend>,
        ttl_secs: u64,
    ) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let spam_lease = format!("{job_id}-spam");
        let mining_lease = format!("{job_id}-mining");
        let interval = Duration::from_secs((ttl_secs / 3).max(1));
        let handle = thread::Builder::new()
            .name(format!("lease-renew-{job_id}"))
            .spawn(move || {
                let mut sequence = 0u64;
                while !thread_stop.load(Ordering::Acquire) {
                    thread::park_timeout(interval);
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    sequence += 1;
                    let spam_result = spam.renew_lease(
                        &spam_lease,
                        LeaseRenewRequest {
                            ttl_secs,
                            request_id: format!("{job_id}-spam-renew-{sequence}"),
                        },
                    );
                    let mining_result = mining.renew_lease(
                        &mining_lease,
                        LeaseRenewRequest {
                            ttl_secs,
                            request_id: format!("{job_id}-mining-renew-{sequence}"),
                        },
                    );
                    let errors: Vec<String> = [
                        spam_result.err().map(|error| format!("spam: {error}")),
                        mining_result.err().map(|error| format!("mining: {error}")),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    if !errors.is_empty() {
                        abort.store(true, Ordering::Release);
                        manager.emit_best_effort(
                            &job_id,
                            "lease_renewal_failed",
                            "lease_renewal_failed",
                            &format!("worker lease renewal failed: {}", errors.join("; ")),
                            None,
                        );
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(handle),
        })
    }

    fn stop(mut self) -> anyhow::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Release);
        let Some(handle) = self.thread.take() else {
            return Ok(());
        };
        handle.thread().unpark();
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("lease renewal thread panicked"))
    }
}

impl Drop for LeaseRenewer {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct OwnedLeaseRenewer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl OwnedLeaseRenewer {
    fn start(
        manager: Arc<JobManager>,
        job_id: String,
        abort: Arc<AtomicBool>,
        ttl_secs: u64,
    ) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let interval = Duration::from_secs((ttl_secs / 3).max(1));
        let handle = thread::Builder::new()
            .name(format!("scenario-lease-renew-{job_id}"))
            .spawn(move || {
                let mut sequence = 0u64;
                while !thread_stop.load(Ordering::Acquire) {
                    thread::park_timeout(interval);
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    sequence = sequence.saturating_add(1);
                    if let Err(error) = manager.renew_owned_leases(&job_id, sequence) {
                        abort.store(true, Ordering::Release);
                        manager.checkpoint_cv.notify_all();
                        manager.emit_best_effort(
                            &job_id,
                            "lease_renewal_failed",
                            "lease_renewal_failed",
                            &format!("scenario worker lease renewal failed: {error}"),
                            None,
                        );
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(handle),
        })
    }

    fn start_for_scenario(
        manager: Arc<JobManager>,
        job_id: String,
        abort: Arc<AtomicBool>,
        ttl_secs: u64,
        runtime: Arc<Mutex<ScenarioRuntime>>,
    ) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let interval = Duration::from_secs((ttl_secs / 3).max(1));
        let handle = thread::Builder::new()
            .name(format!("scenario-lease-renew-{job_id}"))
            .spawn(move || {
                let mut sequence = 0u64;
                while !thread_stop.load(Ordering::Acquire) {
                    thread::park_timeout(interval);
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    sequence = sequence.saturating_add(1);
                    if let Err(error) =
                        manager.renew_scenario_runtime_leases(sequence, runtime.as_ref())
                    {
                        abort.store(true, Ordering::Release);
                        manager.checkpoint_cv.notify_all();
                        manager.emit_best_effort(
                            &job_id,
                            "lease_renewal_failed",
                            "lease_renewal_failed",
                            &format!("scenario worker lease renewal failed: {error}"),
                            None,
                        );
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(handle),
        })
    }

    fn stop(mut self) -> anyhow::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Release);
        let Some(handle) = self.thread.take() else {
            return Ok(());
        };
        handle.thread().unpark();
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("scenario lease renewal thread panicked"))
    }
}

impl Drop for OwnedLeaseRenewer {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Debug)]
struct FaucetRunError {
    code: &'static str,
    message: String,
}

impl FaucetRunError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn aborted() -> Self {
        Self::new("aborted", "faucet operation was aborted")
    }

    fn validation(message: impl Into<String>) -> Self {
        Self::new("validation_failed", message)
    }

    fn validation_error(error: impl std::fmt::Display) -> Self {
        Self::validation(error.to_string())
    }

    fn unavailable(error: impl std::fmt::Display) -> Self {
        Self::new("faucet_unavailable", error.to_string())
    }

    fn insufficient(error: impl std::fmt::Display) -> Self {
        Self::new("insufficient_faucet_funds", error.to_string())
    }

    fn priority(error: impl std::fmt::Display) -> Self {
        Self::new("faucet_priority_invariant_failed", error.to_string())
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    fn internal_error(error: impl std::fmt::Display) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<FaucetRunError> for anyhow::Error {
    fn from(error: FaucetRunError) -> Self {
        anyhow::anyhow!("{}: {}", error.code, error.message)
    }
}

fn normalize_faucet_request(
    mut request: FaucetJobRequest,
    max_request_sats: u64,
) -> Result<FaucetJobRequest, JobManagerError> {
    if request.outputs.is_empty() || request.outputs.len() > FAUCET_MAX_OUTPUTS {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            format!("faucet requires 1 through {FAUCET_MAX_OUTPUTS} outputs"),
        ));
    }
    let mut addresses = HashSet::new();
    let mut total = 0_u64;
    for output in &mut request.outputs {
        output.address = output.address.trim().to_string();
        if output.amount_sats == 0 {
            return Err(JobManagerError::new(
                ErrorCode::ValidationFailed,
                format!("destination {} has a zero amount", output.address),
            ));
        }
        let address =
            bitcoincore_rpc::bitcoin::Address::from_str(&output.address).map_err(|error| {
                JobManagerError::new(
                    ErrorCode::ValidationFailed,
                    format!("invalid destination {}: {error}", output.address),
                )
            })?;
        let checked = simchain_common::require_regtest_address(address).map_err(|error| {
            JobManagerError::new(
                ErrorCode::ValidationFailed,
                format!(
                    "destination {} is not a regtest address: {error}",
                    output.address
                ),
            )
        })?;
        output.address = checked.to_string();
        if !addresses.insert(output.address.clone()) {
            return Err(JobManagerError::new(
                ErrorCode::ValidationFailed,
                format!("duplicate faucet destination {}", output.address),
            ));
        }
        total = total.checked_add(output.amount_sats).ok_or_else(|| {
            JobManagerError::new(ErrorCode::ValidationFailed, "faucet total amount overflow")
        })?;
    }
    if total > max_request_sats {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            format!("faucet total {total} sats exceeds maximum {max_request_sats} sats"),
        ));
    }
    request
        .outputs
        .sort_by(|left, right| left.address.cmp(&right.address));
    Ok(request)
}

fn normalize_required_idempotency_key(key: Option<String>) -> Result<String, JobManagerError> {
    normalize_idempotency_key(key)?.ok_or_else(|| {
        JobManagerError::new(
            ErrorCode::ValidationFailed,
            "Idempotency-Key is required for faucet requests",
        )
    })
}

fn choose_faucet_source(
    requested: FaucetSource,
    preflight: &FaucetPreflight,
    recipient_sats: u64,
    reserve_sats: u64,
) -> Result<(FaucetSourceNode, &[FaucetInput]), FaucetRunError> {
    let node2_total =
        eligible_total(&preflight.node2_inputs).map_err(FaucetRunError::internal_error)?;
    let node3_total =
        eligible_total(&preflight.node3_inputs).map_err(FaucetRunError::internal_error)?;
    let can_fund = |total: u64| {
        total
            .checked_sub(reserve_sats)
            .is_some_and(|available| available >= recipient_sats.saturating_add(546))
    };
    match requested {
        FaucetSource::Node2 if can_fund(node2_total) => {
            Ok((FaucetSourceNode::Node2, &preflight.node2_inputs))
        }
        FaucetSource::Node3 if can_fund(node3_total) => {
            Ok((FaucetSourceNode::Node3, &preflight.node3_inputs))
        }
        FaucetSource::Node2 => Err(FaucetRunError::insufficient(format!(
            "node2 has {node2_total} eligible sats and must retain {reserve_sats} sats"
        ))),
        FaucetSource::Node3 => Err(FaucetRunError::insufficient(format!(
            "node3 has {node3_total} eligible sats and must retain {reserve_sats} sats"
        ))),
        FaucetSource::Auto => {
            let node2_available = node2_total.saturating_sub(reserve_sats);
            let node3_available = node3_total.saturating_sub(reserve_sats);
            if can_fund(node2_total) && node2_available >= node3_available {
                Ok((FaucetSourceNode::Node2, &preflight.node2_inputs))
            } else if can_fund(node3_total) {
                Ok((FaucetSourceNode::Node3, &preflight.node3_inputs))
            } else {
                Err(FaucetRunError::insufficient(format!(
                    "neither miner wallet can fund {recipient_sats} sats while retaining {reserve_sats} sats (node2={node2_total}, node3={node3_total})"
                )))
            }
        }
    }
}

fn normalize_reorg_request(
    mut request: ReorgJobRequest,
) -> Result<ReorgJobRequest, JobManagerError> {
    request.node = request.node.trim().to_ascii_lowercase();
    if request.depth == 0 || request.depth > 100 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "reorg depth must be between 1 and 100",
        ));
    }
    if !matches!(request.node.as_str(), "node2" | "node3") {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "reorg node must be node2 or node3",
        ));
    }
    if request.double_spend_pct > 100 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "double_spend_pct must be between 0 and 100",
        ));
    }
    if request.adds_new_txs > 10_000 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "adds_new_txs must not exceed 10000",
        ));
    }
    Ok(request)
}

fn normalize_mine_request(
    mut request: MineJobRequest,
) -> Result<(MineJobRequest, MinerNode), JobManagerError> {
    request.node = request.node.trim().to_ascii_lowercase();
    let node = parse_miner_node(&request.node)?;
    request.node = node.short_name().to_string();
    if request.blocks == 0 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "blocks must be positive",
        ));
    }
    Ok((request, node))
}

fn normalize_spam_burst_request(
    mut request: SpamBurstJobRequest,
) -> Result<(SpamBurstJobRequest, MinerNode), JobManagerError> {
    request.node = request.node.trim().to_ascii_lowercase();
    let node = parse_miner_node(&request.node)?;
    request.node = node.short_name().to_string();
    if request.txs == 0 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "txs must be positive",
        ));
    }
    Ok((request, node))
}

fn normalize_partition_request(
    mut request: PartitionJobRequest,
) -> Result<(PartitionJobRequest, MinerNode), JobManagerError> {
    request.node = request.node.trim().to_ascii_lowercase();
    let node = parse_miner_node(&request.node)?;
    request.node = node.short_name().to_string();
    if request.main_blocks == 0 || request.isolated_blocks == 0 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "main_blocks and isolated_blocks must be positive",
        ));
    }
    if request.main_blocks == request.isolated_blocks {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "main_blocks and isolated_blocks must differ to guarantee a deterministic winner",
        ));
    }
    if request.main_blocks > 100 || request.isolated_blocks > 100 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "partition branch lengths must not exceed 100 blocks",
        ));
    }
    Ok((request, node))
}

fn normalize_degrade_request(
    mut request: DegradeJobRequest,
) -> Result<(DegradeJobRequest, String), JobManagerError> {
    request.node = request.node.trim().to_ascii_lowercase();
    let node = match request.node.as_str() {
        "node1" | "btc-simnet-node1" => "node1",
        "node2" | "btc-simnet-node2" => "node2",
        "node3" | "btc-simnet-node3" => "node3",
        _ => {
            return Err(JobManagerError::new(
                ErrorCode::ValidationFailed,
                "degrade node must be node1, node2, or node3",
            ))
        }
    };
    request.node = node.to_string();
    if request.delay_ms > 600_000 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "delay_ms must not exceed 600000",
        ));
    }
    if !request.loss_pct.is_finite() || !(0.0..=100.0).contains(&request.loss_pct) {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "loss_pct must be a finite number from 0 through 100",
        ));
    }
    if request.delay_ms == 0 && request.loss_pct == 0.0 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "degrade must specify a positive delay or loss percentage",
        ));
    }
    if request.seconds == 0 || request.seconds > 86_400 {
        return Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "seconds must be between 1 and 86400",
        ));
    }
    Ok((request, node.to_string()))
}

fn parse_miner_node(node: &str) -> Result<MinerNode, JobManagerError> {
    match node {
        "node2" | "btc-simnet-node2" => Ok(MinerNode::Node2),
        "node3" | "btc-simnet-node3" => Ok(MinerNode::Node3),
        _ => Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "node must be node2 or node3",
        )),
    }
}

fn other_miner(node: MinerNode) -> MinerNode {
    match node {
        MinerNode::Node2 => MinerNode::Node3,
        MinerNode::Node3 => MinerNode::Node2,
    }
}

fn network_lease_node(lease: &JobLease) -> anyhow::Result<&str> {
    let node = lease
        .component
        .strip_prefix("network:")
        .ok_or_else(|| anyhow::anyhow!("lease {} is not a network lease", lease.lease_id))?;
    anyhow::ensure!(
        matches!(node, "node1" | "node2" | "node3"),
        "invalid network lease node {node}"
    );
    Ok(node)
}

fn result_hash(value: &Value) -> anyhow::Result<String> {
    value
        .get("last_hash")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("mining result did not include last_hash"))
}

fn successful_cleanup() -> JobCleanup {
    JobCleanup {
        state: CleanupState::Succeeded,
        errors: Vec::new(),
    }
}

fn normalize_idempotency_key(key: Option<String>) -> Result<Option<String>, JobManagerError> {
    let key = key.map(|key| key.trim().to_string());
    match key {
        Some(key) if key.is_empty() => Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "Idempotency-Key must not be empty",
        )),
        Some(key) if key.len() > 200 => Err(JobManagerError::new(
            ErrorCode::ValidationFailed,
            "Idempotency-Key must not exceed 200 bytes",
        )),
        other => Ok(other),
    }
}

fn find_stored<'a>(persisted: &'a PersistedJobs, job_id: &str) -> Option<&'a StoredJob> {
    persisted
        .jobs
        .iter()
        .find(|job| job.detail.summary.id == job_id)
}

fn find_stored_mut<'a>(
    persisted: &'a mut PersistedJobs,
    job_id: &str,
) -> Option<&'a mut StoredJob> {
    persisted
        .jobs
        .iter_mut()
        .find(|job| job.detail.summary.id == job_id)
}

fn owned_leases<'a>(leases: &'a [PauseLease], job_id: &str) -> Vec<&'a PauseLease> {
    leases
        .iter()
        .filter(|lease| lease.owner_job_id == job_id)
        .collect()
}

fn internal_error(error: impl std::fmt::Display) -> JobManagerError {
    JobManagerError::new(ErrorCode::Internal, error.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockBackend;
    use std::sync::atomic::AtomicBool;

    struct BlockingExecutor {
        started: AtomicBool,
        release: AtomicBool,
    }

    struct RecoveryGateExecutor {
        attempted: AtomicBool,
        allow: AtomicBool,
    }

    impl ReorgExecutor for RecoveryGateExecutor {
        fn execute(
            &self,
            _request: &ReorgJobRequest,
            _use_raw_tx_spam: bool,
            _observer: &dyn ReorgObserver,
        ) -> anyhow::Result<ReorgExecution> {
            unreachable!("restart recovery must not resume job execution")
        }

        fn recover(
            &self,
            _request: &ReorgJobRequest,
            _context: &ReorgRecoveryContext,
            _observer: &dyn ReorgObserver,
        ) -> anyhow::Result<()> {
            self.attempted.store(true, Ordering::Release);
            while !self.allow.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }
    }

    impl BlockingExecutor {
        fn new() -> Self {
            Self {
                started: AtomicBool::new(false),
                release: AtomicBool::new(false),
            }
        }
    }

    impl ReorgExecutor for BlockingExecutor {
        fn execute(
            &self,
            request: &ReorgJobRequest,
            _use_raw_tx_spam: bool,
            observer: &dyn ReorgObserver,
        ) -> anyhow::Result<ReorgExecution> {
            self.started.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) && !observer.abort_requested() {
                thread::sleep(Duration::from_millis(5));
            }
            let aborted = observer.abort_requested();
            observer.observe(ReorgProgress {
                phase: ReorgPhase::Completed,
                message: "fake execution complete".to_string(),
                data: None,
            });
            Ok(ReorgExecution {
                result: json!({"depth": request.depth, "aborted": aborted}),
                chain_changed: false,
                aborted,
            })
        }

        fn recover(
            &self,
            _request: &ReorgJobRequest,
            _context: &ReorgRecoveryContext,
            _observer: &dyn ReorgObserver,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn manager(
        dir: &std::path::Path,
        executor: Arc<BlockingExecutor>,
    ) -> (Arc<MockBackend>, Arc<JobManager>) {
        let backend = Arc::new(MockBackend::new());
        backend.sync_workers();
        let control_store = ControlStateStore::open(dir.to_path_buf()).expect("control store");
        let control_state = Arc::new(RwLock::new(
            control_store
                .load_or_initialize(ControlState::default().desired)
                .expect("control state"),
        ));
        let apply_lock = Arc::new(Mutex::new(()));
        let manager = JobManager::open_with_ttl(
            dir,
            JobDependencies {
                mining: backend.clone(),
                spam: backend.clone(),
                network: backend.clone(),
                chain: backend.clone(),
                control_store,
                control_state,
                apply_lock,
                reorg: executor,
                scenario: backend.clone(),
                network_actions: backend.clone(),
                faucet: backend.clone(),
                faucet_settings: test_faucet_settings(),
            },
            60,
        )
        .expect("job manager");
        (backend, manager)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(std::time::Instant::now() < deadline, "condition timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn test_faucet_settings() -> FaucetSettings {
        FaucetSettings {
            node2_wallet_name: "node2".to_string(),
            node3_wallet_name: "node3".to_string(),
            wallet_reserve_sats: 60_000_000_000,
            max_request_sats: 10_000_000_000,
            explorer_url: "http://127.0.0.1:1080".to_string(),
        }
    }

    fn control_fixture(
        dir: &std::path::Path,
    ) -> (ControlStateStore, Arc<RwLock<ControlState>>, Arc<Mutex<()>>) {
        let control_store = ControlStateStore::open(dir.to_path_buf()).expect("control store");
        let control_state = Arc::new(RwLock::new(
            control_store
                .load_or_initialize(ControlState::default().desired)
                .expect("control state"),
        ));
        (control_store, control_state, Arc::new(Mutex::new(())))
    }

    #[test]
    fn one_mutation_idempotency_and_event_cursors_are_pinned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (_backend, manager) = manager(dir.path(), executor.clone());
        let request = ReorgJobRequest::default();
        let created = manager
            .start_reorg(request.clone(), Some("retry-key".to_string()), true)
            .expect("start");
        wait_until(|| executor.started.load(Ordering::Acquire));

        let reused = manager
            .start_reorg(request, Some("retry-key".to_string()), true)
            .expect("idempotent retry");
        assert!(reused.reused);
        assert_eq!(reused.job_id, created.job_id);

        let conflict = manager
            .start_reorg(
                ReorgJobRequest {
                    depth: 4,
                    ..ReorgJobRequest::default()
                },
                Some("other-key".to_string()),
                true,
            )
            .expect_err("second mutation must conflict");
        assert_eq!(conflict.code, ErrorCode::OperationInProgress);
        assert_eq!(
            conflict.active_job_id.as_deref(),
            Some(created.job_id.as_str())
        );

        executor.release.store(true, Ordering::Release);
        wait_until(|| {
            manager
                .get(&created.job_id)
                .expect("job")
                .summary
                .state
                .is_terminal()
        });
        let job = manager.get(&created.job_id).expect("job");
        assert_eq!(job.summary.state, JobState::Succeeded);
        assert_eq!(job.summary.cleanup.state, CleanupState::Succeeded);
        assert!(manager.list().active_job_id.is_none());

        let events = manager
            .events(Some(&created.job_id), 0, 500)
            .expect("events");
        assert!(!events.events.is_empty());
        assert!(events
            .events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
        let tail = manager
            .events(Some(&created.job_id), events.events[0].sequence, 500)
            .expect("cursor events");
        assert!(tail
            .events
            .iter()
            .all(|event| event.sequence > events.events[0].sequence));
    }

    #[test]
    fn abort_is_cooperative_and_cleanup_is_reported_separately() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (_backend, manager) = manager(dir.path(), executor.clone());
        let created = manager
            .start_reorg(ReorgJobRequest::default(), None, true)
            .expect("start");
        wait_until(|| executor.started.load(Ordering::Acquire));
        let response = manager.abort(&created.job_id).expect("abort");
        assert_eq!(response.state, JobState::AbortRequested);
        wait_until(|| {
            manager
                .get(&created.job_id)
                .expect("job")
                .summary
                .state
                .is_terminal()
        });
        let job = manager.get(&created.job_id).expect("job");
        assert_eq!(job.summary.state, JobState::Aborted);
        assert_eq!(job.summary.cleanup.state, CleanupState::Succeeded);
    }

    #[test]
    fn restart_marks_active_job_interrupted_and_recovers_before_unlocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(MockBackend::new());
        backend.sync_workers();
        let store = JobStore::open(dir.path()).expect("store");
        let job_id = "job-restarted".to_string();
        store
            .save(&PersistedJobs {
                schema_version: JOB_SCHEMA_VERSION,
                next_event_sequence: 1,
                next_checkpoint_generation: 1,
                active_job_id: Some(job_id.clone()),
                jobs: vec![StoredJob {
                    detail: JobDetail {
                        summary: JobSummary {
                            id: job_id.clone(),
                            kind: JobKind::Reorg,
                            state: JobState::Running,
                            phase: "invalidated".to_string(),
                            created_at_ms: 1,
                            started_at_ms: Some(2),
                            ended_at_ms: None,
                            cleanup: JobCleanup::default(),
                        },
                        request: json!({"depth": 3}),
                        leases: vec![JobLease {
                            component: "spam".to_string(),
                            lease_id: "job-restarted-spam".to_string(),
                            purpose: "reorg".to_string(),
                        }],
                        current_step: None,
                        checkpoints: Vec::new(),
                        result: None,
                        failure: None,
                    },
                    idempotency_key: None,
                    request_fingerprint: "request".to_string(),
                    faucet_recovery: None,
                    reorg_recovery: ReorgRecoveryContext {
                        mutation_may_have_occurred: true,
                        request: Some(ReorgJobRequest::default()),
                        invalidated_block_hash: Some("00".repeat(32)),
                    },
                }],
            })
            .expect("seed active job");

        let (control_store, control_state, apply_lock) = control_fixture(dir.path());
        let manager = JobManager::open_with_ttl(
            dir.path(),
            JobDependencies {
                mining: backend.clone(),
                spam: backend.clone(),
                network: backend.clone(),
                chain: backend.clone(),
                control_store,
                control_state,
                apply_lock,
                reorg: Arc::new(BlockingExecutor::new()),
                scenario: backend.clone(),
                network_actions: backend.clone(),
                faucet: backend,
                faucet_settings: test_faucet_settings(),
            },
            60,
        )
        .expect("reopen");
        assert_eq!(
            manager.get(&job_id).expect("job").summary.state,
            JobState::Interrupted
        );
        wait_until(|| manager.list().active_job_id.is_none());
        let job = manager.get(&job_id).expect("job");
        assert_eq!(job.summary.state, JobState::Interrupted);
        assert_eq!(job.summary.cleanup.state, CleanupState::Succeeded);
    }

    #[test]
    fn restart_keeps_leases_and_mutation_lock_until_chain_recovery_is_safe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(MockBackend::new());
        backend.sync_workers();
        let job_id = "job-recovery-gated".to_string();
        MiningControlBackend::acquire_lease(
            backend.as_ref(),
            LeaseRequest {
                lease_id: format!("{job_id}-mining"),
                owner_job_id: job_id.clone(),
                purpose: "reorg".to_string(),
                ttl_secs: 60,
                request_id: "seed-mining".to_string(),
            },
        )
        .expect("seed mining lease");
        SpamControlBackend::acquire_lease(
            backend.as_ref(),
            LeaseRequest {
                lease_id: format!("{job_id}-spam"),
                owner_job_id: job_id.clone(),
                purpose: "reorg".to_string(),
                ttl_secs: 60,
                request_id: "seed-spam".to_string(),
            },
        )
        .expect("seed spam lease");
        JobStore::open(dir.path())
            .expect("store")
            .save(&PersistedJobs {
                schema_version: JOB_SCHEMA_VERSION,
                next_event_sequence: 1,
                next_checkpoint_generation: 1,
                active_job_id: Some(job_id.clone()),
                jobs: vec![StoredJob {
                    detail: JobDetail {
                        summary: JobSummary {
                            id: job_id.clone(),
                            kind: JobKind::Reorg,
                            state: JobState::Running,
                            phase: "invalidated".to_string(),
                            created_at_ms: 1,
                            started_at_ms: Some(2),
                            ended_at_ms: None,
                            cleanup: JobCleanup::default(),
                        },
                        request: serde_json::to_value(ReorgJobRequest::default())
                            .expect("request JSON"),
                        leases: Vec::new(),
                        current_step: None,
                        checkpoints: Vec::new(),
                        result: None,
                        failure: None,
                    },
                    idempotency_key: None,
                    request_fingerprint: "request".to_string(),
                    faucet_recovery: None,
                    reorg_recovery: ReorgRecoveryContext {
                        mutation_may_have_occurred: true,
                        request: Some(ReorgJobRequest::default()),
                        invalidated_block_hash: Some("00".repeat(32)),
                    },
                }],
            })
            .expect("seed job");
        let executor = Arc::new(RecoveryGateExecutor {
            attempted: AtomicBool::new(false),
            allow: AtomicBool::new(false),
        });
        let (control_store, control_state, apply_lock) = control_fixture(dir.path());
        let manager = JobManager::open_with_ttl(
            dir.path(),
            JobDependencies {
                mining: backend.clone(),
                spam: backend.clone(),
                network: backend.clone(),
                chain: backend.clone(),
                control_store,
                control_state,
                apply_lock,
                reorg: executor.clone(),
                scenario: backend.clone(),
                network_actions: backend.clone(),
                faucet: backend.clone(),
                faucet_settings: test_faucet_settings(),
            },
            60,
        )
        .expect("reopen");
        wait_until(|| executor.attempted.load(Ordering::Acquire));
        assert_eq!(
            manager.list().active_job_id.as_deref(),
            Some(job_id.as_str())
        );
        assert!(!MiningControlBackend::status(backend.as_ref())
            .expect("mining status")
            .active_leases
            .is_empty());
        assert!(!SpamControlBackend::status(backend.as_ref())
            .expect("spam status")
            .active_leases
            .is_empty());

        executor.allow.store(true, Ordering::Release);
        wait_until(|| manager.list().active_job_id.is_none());
        assert!(MiningControlBackend::status(backend.as_ref())
            .expect("mining status")
            .active_leases
            .is_empty());
        assert!(SpamControlBackend::status(backend.as_ref())
            .expect("spam status")
            .active_leases
            .is_empty());
    }

    #[test]
    fn restart_interrupts_a_held_scenario_and_recovers_its_owned_lease() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(MockBackend::new());
        backend.sync_workers();
        let job_id = "job-held-scenario".to_string();
        let lease_id = format!("{job_id}-mining-1");
        MiningControlBackend::acquire_lease(
            backend.as_ref(),
            LeaseRequest {
                lease_id: lease_id.clone(),
                owner_job_id: job_id.clone(),
                purpose: "scenario pause_mining step".to_string(),
                ttl_secs: 60,
                request_id: "seed-held-scenario".to_string(),
            },
        )
        .expect("seed mining lease");
        JobStore::open(dir.path())
            .expect("store")
            .save(&PersistedJobs {
                schema_version: JOB_SCHEMA_VERSION,
                next_event_sequence: 1,
                next_checkpoint_generation: 2,
                active_job_id: Some(job_id.clone()),
                jobs: vec![StoredJob {
                    detail: JobDetail {
                        summary: JobSummary {
                            id: job_id.clone(),
                            kind: JobKind::Scenario,
                            state: JobState::WaitingAtCheckpoint,
                            phase: "waiting_at_checkpoint".to_string(),
                            created_at_ms: 1,
                            started_at_ms: Some(2),
                            ended_at_ms: None,
                            cleanup: JobCleanup::default(),
                        },
                        request: json!({"version": 1, "steps": []}),
                        leases: vec![JobLease {
                            component: "mining".to_string(),
                            lease_id,
                            purpose: "scenario pause_mining step".to_string(),
                        }],
                        current_step: Some(ScenarioStepStatus {
                            index: 2,
                            total: 2,
                            kind: "checkpoint".to_string(),
                            state: "waiting".to_string(),
                        }),
                        checkpoints: vec![JobCheckpoint {
                            name: "held".to_string(),
                            generation: 1,
                            state: CheckpointState::Reached,
                            pause: true,
                            timeout_secs: Some(60),
                            step_index: 2,
                            arrived_at_ms: Some(3),
                            released_at_ms: None,
                            live_summary: Some(json!({"height": 204})),
                        }],
                        result: None,
                        failure: None,
                    },
                    idempotency_key: None,
                    request_fingerprint: "scenario".to_string(),
                    faucet_recovery: None,
                    reorg_recovery: ReorgRecoveryContext::default(),
                }],
            })
            .expect("seed held scenario");

        let (control_store, control_state, apply_lock) = control_fixture(dir.path());
        let manager = JobManager::open_with_ttl(
            dir.path(),
            JobDependencies {
                mining: backend.clone(),
                spam: backend.clone(),
                network: backend.clone(),
                chain: backend.clone(),
                control_store,
                control_state,
                apply_lock,
                reorg: Arc::new(BlockingExecutor::new()),
                scenario: backend.clone(),
                network_actions: backend.clone(),
                faucet: backend.clone(),
                faucet_settings: test_faucet_settings(),
            },
            60,
        )
        .expect("reopen");
        assert_eq!(
            manager.get(&job_id).expect("job").summary.state,
            JobState::Interrupted
        );
        wait_until(|| manager.list().active_job_id.is_none());
        assert!(MiningControlBackend::status(backend.as_ref())
            .expect("mining status")
            .active_leases
            .is_empty());
        let job = manager.get(&job_id).expect("job");
        assert_eq!(job.summary.cleanup.state, CleanupState::Succeeded);
        assert_eq!(job.checkpoints[0].state, CheckpointState::Reached);
    }

    #[test]
    fn scenario_checkpoint_is_durable_generation_safe_and_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor);
        let yaml = r#"
version: 1
steps:
  - type: pause_mining
  - type: checkpoint
    name: mempool_loaded
    timeout_secs: 5
  - type: resume_mining
"#;
        let created = manager
            .start_scenario(yaml.to_string(), Some("scenario-retry".to_string()), true)
            .expect("start scenario");
        let reused = manager
            .start_scenario(yaml.to_string(), Some("scenario-retry".to_string()), true)
            .expect("idempotent retry");
        assert!(reused.reused);
        assert_eq!(reused.job_id, created.job_id);

        wait_until(|| {
            manager
                .checkpoint(&created.job_id, "mempool_loaded")
                .is_ok_and(|response| response.checkpoint.state == CheckpointState::Reached)
        });
        let held = manager.get(&created.job_id).expect("held job");
        assert_eq!(held.summary.state, JobState::WaitingAtCheckpoint);
        let checkpoint = manager
            .checkpoint(&created.job_id, "mempool_loaded")
            .expect("checkpoint")
            .checkpoint;
        assert!(checkpoint.generation > 0);
        assert!(checkpoint.live_summary.is_some());
        assert_eq!(
            MiningControlBackend::status(backend.as_ref())
                .expect("mining status")
                .active_leases
                .len(),
            1
        );

        let stale = manager
            .release_checkpoint(
                &created.job_id,
                "mempool_loaded",
                ReleaseCheckpointRequest {
                    generation: checkpoint.generation + 1,
                },
            )
            .expect_err("stale generation");
        assert_eq!(stale.code, ErrorCode::CheckpointConflict);

        let released = manager
            .release_checkpoint(
                &created.job_id,
                "mempool_loaded",
                ReleaseCheckpointRequest {
                    generation: checkpoint.generation,
                },
            )
            .expect("release checkpoint");
        let replay = manager
            .release_checkpoint(
                &created.job_id,
                "mempool_loaded",
                ReleaseCheckpointRequest {
                    generation: checkpoint.generation,
                },
            )
            .expect("idempotent release");
        assert_eq!(released.checkpoint, replay.checkpoint);

        wait_until(|| {
            manager
                .get(&created.job_id)
                .expect("job")
                .summary
                .state
                .is_terminal()
        });
        let job = manager.get(&created.job_id).expect("job");
        assert_eq!(job.summary.state, JobState::Succeeded);
        assert!(job.result.is_some());
        assert!(manager.list().active_job_id.is_none());
        assert!(MiningControlBackend::status(backend.as_ref())
            .expect("mining status")
            .active_leases
            .is_empty());
    }

    #[test]
    fn checkpoint_timeout_fails_scenario_and_cleans_owned_leases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor);
        let created = manager
            .start_scenario(
                r#"
version: 1
steps:
  - type: pause_mining
  - type: checkpoint
    name: abandoned
    timeout_secs: 1
"#
                .to_string(),
                None,
                true,
            )
            .expect("start scenario");
        wait_until(|| {
            manager
                .checkpoint(&created.job_id, "abandoned")
                .is_ok_and(|response| response.checkpoint.state == CheckpointState::Reached)
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !manager
            .get(&created.job_id)
            .expect("job")
            .summary
            .state
            .is_terminal()
        {
            assert!(Instant::now() < deadline, "scenario timeout did not finish");
            thread::sleep(Duration::from_millis(10));
        }
        let job = manager.get(&created.job_id).expect("job");
        assert_eq!(job.summary.state, JobState::Failed);
        assert_eq!(job.summary.cleanup.state, CleanupState::Succeeded);
        assert_eq!(job.checkpoints[0].state, CheckpointState::TimedOut);
        assert!(manager.list().active_job_id.is_none());
        assert!(MiningControlBackend::status(backend.as_ref())
            .expect("mining status")
            .active_leases
            .is_empty());
    }

    #[test]
    fn scenario_runtime_renewal_skips_workers_without_active_leases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor);
        {
            let mut world = backend.world.lock().expect("world");
            world.mining_status_fail_times = 1;
            world.spam_status_fail_times = 1;
        }

        let runtime = Mutex::new(ScenarioRuntime::default());
        manager
            .renew_scenario_runtime_leases(1, &runtime)
            .expect("no active leases means no worker status dependency");

        let world = backend.world.lock().expect("world");
        assert_eq!(world.mining_status_fail_times, 1);
        assert_eq!(world.spam_status_fail_times, 1);
    }

    #[test]
    fn abort_wakes_a_held_scenario_and_runs_owned_cleanup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor);
        let created = manager
            .start_scenario(
                r#"
version: 1
steps:
  - type: pause_mining
  - type: checkpoint
    name: held
    timeout_secs: 30
"#
                .to_string(),
                None,
                true,
            )
            .expect("start scenario");
        wait_until(|| {
            manager
                .checkpoint(&created.job_id, "held")
                .is_ok_and(|response| response.checkpoint.state == CheckpointState::Reached)
        });
        manager.abort(&created.job_id).expect("abort");
        wait_until(|| {
            manager
                .get(&created.job_id)
                .expect("job")
                .summary
                .state
                .is_terminal()
        });
        let job = manager.get(&created.job_id).expect("job");
        assert_eq!(job.summary.state, JobState::Aborted);
        assert_eq!(job.summary.cleanup.state, CleanupState::Succeeded);
        let release = manager
            .release_checkpoint(
                &created.job_id,
                "held",
                ReleaseCheckpointRequest {
                    generation: job.checkpoints[0].generation,
                },
            )
            .expect_err("terminal checkpoint occurrence is stale");
        assert_eq!(release.code, ErrorCode::CheckpointConflict);
        assert!(MiningControlBackend::status(backend.as_ref())
            .expect("mining status")
            .active_leases
            .is_empty());
    }

    #[test]
    fn existing_v1_scenarios_run_through_domain_actions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (_backend, manager) = manager(dir.path(), executor);
        let created = manager
            .start_scenario(
                r#"
version: 1
steps:
  - type: wait_height
    height: 204
  - type: wait_tx
    txid: "1111111111111111111111111111111111111111111111111111111111111111"
    state: confirmed
    confirmations: 2
  - type: pause_mining
  - type: mine
    node: btc-simnet-node2
    blocks: 1
  - type: spam_burst
    node: btc-simnet-node2
    txs: 2
    outputs_per_tx: 1
  - type: partition
    node: btc-simnet-node3
    main_blocks: 1
    isolated_blocks: 2
  - type: resume_mining
"#
                .to_string(),
                None,
                true,
            )
            .expect("start scenario");
        wait_until(|| {
            manager
                .get(&created.job_id)
                .expect("job")
                .summary
                .state
                .is_terminal()
        });
        assert_eq!(
            manager.get(&created.job_id).expect("job").summary.state,
            JobState::Succeeded
        );
    }

    #[test]
    fn hot_control_scenario_steps_run_inline_under_the_scenario_job() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor);
        let created = manager
            .start_scenario(
                r#"
version: 1
steps:
  - type: set_config
    settings:
      BLOCK_INTERVAL_MODE: fixed
      BLOCK_INTERVAL_MEAN_SECS: 10
      FALLBACK_FEE: 0.0002
  - type: assert_config
    effective: true
    settings:
      BLOCK_INTERVAL_MODE: fixed
      BLOCK_INTERVAL_MEAN_SECS: 10
      FALLBACK_FEE: 0.0002
  - type: degrade
    node: node2
    delay_ms: 1
    seconds: 1
  - type: faucet
    source: auto
    wait_confirmed: false
    outputs:
      - address: bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr
        amount: 1btc
"#
                .to_string(),
                None,
                true,
            )
            .expect("start scenario");
        wait_until(|| {
            manager
                .get(&created.job_id)
                .expect("job")
                .summary
                .state
                .is_terminal()
        });
        let job = manager.get(&created.job_id).expect("job");
        assert_eq!(job.summary.state, JobState::Succeeded);
        assert_eq!(job.summary.cleanup.state, CleanupState::Succeeded);
        assert!(manager.list().active_job_id.is_none());
        assert_eq!(
            NetworkControlBackend::status(backend.as_ref(), "node2")
                .expect("node2 network")
                .active_lease,
            None
        );
        assert!(manager.faucet_status().pending_transfer.is_some());
    }

    #[test]
    fn job_store_v1_migrates_to_v2_without_faucet_recovery_material() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JobStore::open(dir.path()).expect("store");
        store
            .save(&PersistedJobsV1 {
                schema_version: 1,
                next_event_sequence: 9,
                next_checkpoint_generation: 4,
                active_job_id: None,
                jobs: vec![StoredJobV1 {
                    detail: JobDetail {
                        summary: JobSummary {
                            id: "job-v1".to_string(),
                            kind: JobKind::Reorg,
                            state: JobState::Succeeded,
                            phase: "succeeded".to_string(),
                            created_at_ms: 1,
                            started_at_ms: Some(2),
                            ended_at_ms: Some(3),
                            cleanup: successful_cleanup(),
                        },
                        request: json!({"depth": 3}),
                        leases: Vec::new(),
                        current_step: None,
                        checkpoints: Vec::new(),
                        result: None,
                        failure: None,
                    },
                    idempotency_key: Some("v1-key".to_string()),
                    request_fingerprint: "v1-request".to_string(),
                    reorg_recovery: ReorgRecoveryContext::default(),
                }],
            })
            .expect("seed v1 index");

        let migrated = load_and_migrate_jobs(&store).expect("migrate v1 index");
        assert_eq!(migrated.schema_version, JOB_SCHEMA_VERSION);
        assert_eq!(migrated.next_event_sequence, 9);
        assert_eq!(migrated.next_checkpoint_generation, 4);
        assert_eq!(migrated.jobs.len(), 1);
        assert!(migrated.jobs[0].faucet_recovery.is_none());
        let persisted = store
            .load_optional::<Value>()
            .expect("load migrated index")
            .expect("migrated index");
        assert_eq!(persisted["schema_version"], JOB_SCHEMA_VERSION);
        assert!(persisted["jobs"][0].get("faucet_recovery").is_none());
    }

    #[test]
    fn manual_mine_and_spam_burst_are_bounded_owned_jobs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor);
        let mine = manager
            .start_mine(
                MineJobRequest {
                    node: "btc-simnet-node2".to_string(),
                    blocks: 2,
                },
                Some("mine-retry".to_string()),
            )
            .expect("start mine");
        let reused = manager
            .start_mine(
                MineJobRequest {
                    node: "node2".to_string(),
                    blocks: 2,
                },
                Some("mine-retry".to_string()),
            )
            .expect("normalized idempotent retry");
        assert!(reused.reused);
        assert_eq!(reused.job_id, mine.job_id);
        wait_until(|| {
            manager
                .get(&mine.job_id)
                .expect("mine job")
                .summary
                .state
                .is_terminal()
        });
        let mine_detail = manager.get(&mine.job_id).expect("mine job");
        assert_eq!(mine_detail.summary.kind, JobKind::Mine);
        assert_eq!(mine_detail.summary.state, JobState::Succeeded);
        assert_eq!(mine_detail.request["node"], "node2");
        assert_eq!(mine_detail.result.expect("mine result")["blocks"], 2);

        let burst = manager
            .start_spam_burst(
                SpamBurstJobRequest {
                    node: "node3".to_string(),
                    txs: 3,
                    outputs_per_tx: 2,
                },
                None,
            )
            .expect("start burst");
        wait_until(|| {
            manager
                .get(&burst.job_id)
                .expect("burst job")
                .summary
                .state
                .is_terminal()
        });
        let burst_detail = manager.get(&burst.job_id).expect("burst job");
        assert_eq!(burst_detail.summary.kind, JobKind::SpamBurst);
        assert_eq!(burst_detail.summary.state, JobState::Succeeded);
        assert_eq!(
            burst_detail.result.expect("burst result")["accepted_transactions"],
            3
        );
        assert!(MiningControlBackend::status(backend.as_ref())
            .expect("mining status")
            .active_leases
            .is_empty());
        assert!(SpamControlBackend::status(backend.as_ref())
            .expect("spam status")
            .active_leases
            .is_empty());
    }

    #[test]
    fn partition_and_degrade_jobs_use_owned_network_leases_and_heal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor);
        let partition = manager
            .start_partition(
                PartitionJobRequest {
                    node: "btc-simnet-node3".to_string(),
                    main_blocks: 2,
                    isolated_blocks: 3,
                },
                Some("partition-retry".to_string()),
            )
            .expect("start partition");
        wait_until(|| {
            manager
                .get(&partition.job_id)
                .expect("partition")
                .summary
                .state
                .is_terminal()
        });
        let detail = manager.get(&partition.job_id).expect("partition");
        assert_eq!(detail.summary.kind, JobKind::Partition);
        assert_eq!(detail.summary.state, JobState::Succeeded);
        assert_eq!(detail.summary.cleanup.state, CleanupState::Succeeded);
        assert_eq!(detail.request["node"], "node3");
        assert_eq!(detail.result.expect("result")["expected_tip"], "node3-3");
        assert!(NetworkControlBackend::status(backend.as_ref(), "node3")
            .expect("network status")
            .active_lease
            .is_none());
        assert!(MiningControlBackend::status(backend.as_ref())
            .expect("mining")
            .active_leases
            .is_empty());
        assert!(SpamControlBackend::status(backend.as_ref())
            .expect("spam")
            .active_leases
            .is_empty());

        let degrade = manager
            .start_degrade(
                DegradeJobRequest {
                    node: "node1".to_string(),
                    delay_ms: 100,
                    loss_pct: 0.5,
                    seconds: 30,
                },
                None,
            )
            .expect("start degrade");
        wait_until(|| {
            NetworkControlBackend::status(backend.as_ref(), "node1")
                .expect("network status")
                .active_lease
                .is_some()
        });
        manager.abort(&degrade.job_id).expect("abort degrade");
        wait_until(|| {
            manager
                .get(&degrade.job_id)
                .expect("degrade")
                .summary
                .state
                .is_terminal()
        });
        let detail = manager.get(&degrade.job_id).expect("degrade");
        assert_eq!(detail.summary.state, JobState::Aborted);
        assert_eq!(detail.summary.cleanup.state, CleanupState::Succeeded);
        assert!(NetworkControlBackend::status(backend.as_ref(), "node1")
            .expect("network status")
            .active_lease
            .is_none());
    }

    #[test]
    fn lost_network_acquire_response_still_heals_persisted_lease_intent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor);
        backend
            .world
            .lock()
            .expect("world lock")
            .network_acquire_response_fail_times = 1;

        let partition = manager
            .start_partition(
                PartitionJobRequest {
                    node: "node3".to_string(),
                    main_blocks: 2,
                    isolated_blocks: 3,
                },
                None,
            )
            .expect("start partition");
        wait_until(|| {
            manager
                .get(&partition.job_id)
                .expect("partition")
                .summary
                .state
                .is_terminal()
        });

        let detail = manager.get(&partition.job_id).expect("partition");
        assert_eq!(detail.summary.state, JobState::Failed);
        assert_eq!(detail.summary.cleanup.state, CleanupState::Succeeded);
        assert!(detail
            .leases
            .iter()
            .any(|lease| lease.component == "network:node3"));
        assert!(NetworkControlBackend::status(backend.as_ref(), "node3")
            .expect("network status")
            .active_lease
            .is_none());
        assert!(MiningControlBackend::status(backend.as_ref())
            .expect("mining")
            .active_leases
            .is_empty());
        assert!(SpamControlBackend::status(backend.as_ref())
            .expect("spam")
            .active_leases
            .is_empty());
    }

    #[test]
    fn invalid_requests_do_not_reserve_the_coordinator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (_backend, manager) = manager(dir.path(), executor);
        let error = manager
            .start_reorg(
                ReorgJobRequest {
                    depth: 0,
                    ..ReorgJobRequest::default()
                },
                None,
                true,
            )
            .expect_err("invalid depth");
        assert_eq!(error.code, ErrorCode::ValidationFailed);
        assert!(manager.list().active_job_id.is_none());
        assert!(manager.list().jobs.is_empty());

        let error = manager
            .start_mine(
                MineJobRequest {
                    node: "node2".to_string(),
                    blocks: 0,
                },
                None,
            )
            .expect_err("zero blocks");
        assert_eq!(error.code, ErrorCode::ValidationFailed);
        let error = manager
            .start_spam_burst(
                SpamBurstJobRequest {
                    node: "node2".to_string(),
                    txs: 0,
                    outputs_per_tx: 0,
                },
                None,
            )
            .expect_err("zero txs");
        assert_eq!(error.code, ErrorCode::ValidationFailed);
        assert!(manager.list().active_job_id.is_none());
        assert!(manager.list().jobs.is_empty());
    }
}
