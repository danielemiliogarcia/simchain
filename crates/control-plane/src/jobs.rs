//! Single-mutation job coordinator, persistence, events, abort, worker leases,
//! and restart recovery.

use crate::backend::{MiningControlBackend, SpamControlBackend};
use crate::job_store::JobStore;
use crate::reorg_job::{ReorgExecution, ReorgExecutor, ReorgRecoveryContext};
use crate::scenario_job::ScenarioActionBackend;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use simchain_common::control_api::{
    AbortJobResponse, CheckpointState, CleanupState, ErrorCode, JobCheckpoint,
    JobCheckpointResponse, JobCleanup, JobCreatedResponse, JobDetail, JobEvent, JobEventsResponse,
    JobFailure, JobKind, JobLease, JobListResponse, JobState, JobSummary, MineJobRequest,
    ReleaseCheckpointRequest, ReorgJobRequest, ScenarioStepStatus, SpamBurstJobRequest,
};
use simchain_common::internal_api::{
    LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest, PauseLease,
};
use simchain_reorg::{ReorgObserver, ReorgPhase, ReorgProgress};
use simchain_scenario_engine::{
    CheckpointStep, MinerNode, Scenario, ScenarioActions, ScenarioControl, ScenarioProgress,
    ScenarioProgressPhase, Step,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const JOB_SCHEMA_VERSION: u32 = 1;
const MAX_JOB_HISTORY: usize = 100;
const EVENT_RING_CAPACITY: usize = 2_048;
const DEFAULT_LEASE_TTL_SECS: u64 = 120;
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
    #[serde(default)]
    reorg_recovery: ReorgRecoveryContext,
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

struct ManagerState {
    persisted: PersistedJobs,
    events: VecDeque<JobEvent>,
    aborts: HashMap<String, Arc<AtomicBool>>,
    recovering: HashSet<String>,
    recovery_errors: HashMap<String, String>,
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
    reorg: Arc<dyn ReorgExecutor>,
    scenario: Arc<dyn ScenarioActionBackend>,
    checkpoint_cv: Condvar,
    id_sequence: AtomicU64,
    lease_ttl_secs: u64,
}

impl JobManager {
    pub fn open(
        state_dir: &std::path::Path,
        mining: Arc<dyn MiningControlBackend>,
        spam: Arc<dyn SpamControlBackend>,
        reorg: Arc<dyn ReorgExecutor>,
        scenario: Arc<dyn ScenarioActionBackend>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open_with_ttl(
            state_dir,
            mining,
            spam,
            reorg,
            scenario,
            DEFAULT_LEASE_TTL_SECS,
        )
    }

    fn open_with_ttl(
        state_dir: &std::path::Path,
        mining: Arc<dyn MiningControlBackend>,
        spam: Arc<dyn SpamControlBackend>,
        reorg: Arc<dyn ReorgExecutor>,
        scenario: Arc<dyn ScenarioActionBackend>,
        lease_ttl_secs: u64,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(lease_ttl_secs > 0, "job lease TTL must be positive");
        let store = JobStore::open(state_dir)?;
        let mut persisted: PersistedJobs = store.load()?;
        anyhow::ensure!(
            persisted.schema_version == JOB_SCHEMA_VERSION,
            "unsupported job schema {} (expected {JOB_SCHEMA_VERSION})",
            persisted.schema_version
        );

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
                job.detail.summary.phase = "recovering_worker_leases".to_string();
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
            }),
            mining,
            spam,
            reorg,
            scenario,
            checkpoint_cv: Condvar::new(),
            id_sequence: AtomicU64::new(1),
            lease_ttl_secs,
        });
        if let Some(job_id) = recovery_job {
            manager.emit_best_effort(
                &job_id,
                "restart_recovery",
                "recovering_worker_leases",
                "previous active job was marked interrupted; recovering owned leases",
                None,
            );
            manager.spawn_recovery(job_id);
        }
        Ok(manager)
    }

    pub fn ensure_idle(&self) -> Result<(), JobManagerError> {
        let state = self.state.lock().expect("job manager lock");
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
            self.get(job_id)?;
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
            runtime: Mutex::new(ScenarioRuntime::default()),
        };
        let renewer = match OwnedLeaseRenewer::start(
            self.clone(),
            job_id.clone(),
            abort.clone(),
            self.lease_ttl_secs,
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

    fn acquire_spam_lease(&self, job_id: &str, leases: &mut Vec<JobLease>) -> anyhow::Result<()> {
        let lease = JobLease {
            component: "spam".to_string(),
            lease_id: format!("{job_id}-spam"),
            purpose: "reorg chain mutation".to_string(),
        };
        self.spam.acquire_lease(LeaseRequest {
            lease_id: lease.lease_id.clone(),
            owner_job_id: job_id.to_string(),
            purpose: lease.purpose.clone(),
            ttl_secs: self.lease_ttl_secs,
            request_id: format!("{job_id}-spam-acquire"),
        })?;
        leases.push(lease.clone());
        self.record_lease(job_id, lease);
        Ok(())
    }

    fn acquire_mining_lease(&self, job_id: &str, leases: &mut Vec<JobLease>) -> anyhow::Result<()> {
        let lease = JobLease {
            component: "mining".to_string(),
            lease_id: format!("{job_id}-mining"),
            purpose: "reorg chain mutation".to_string(),
        };
        self.mining.acquire_lease(LeaseRequest {
            lease_id: lease.lease_id.clone(),
            owner_job_id: job_id.to_string(),
            purpose: lease.purpose.clone(),
            ttl_secs: self.lease_ttl_secs,
            request_id: format!("{job_id}-mining-acquire"),
        })?;
        leases.push(lease.clone());
        self.record_lease(job_id, lease);
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
        match component {
            "spam" => self.spam.acquire_lease(request)?,
            "mining" => self.mining.acquire_lease(request)?,
            other => anyhow::bail!("unknown lease component {other}"),
        };
        self.record_lease(job_id, lease.clone());
        Ok(lease)
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

    fn record_lease(&self, job_id: &str, lease: JobLease) {
        let component = lease.component.clone();
        let mut state = self.state.lock().expect("job manager lock");
        if let Some(job) = find_stored_mut(&mut state.persisted, job_id) {
            if !job
                .detail
                .leases
                .iter()
                .any(|existing| existing.lease_id == lease.lease_id)
            {
                job.detail.leases.push(lease.clone());
            }
            if let Err(error) = self.store.save(&state.persisted) {
                tracing::error!(job_id, "failed to persist acquired lease: {error}");
            }
        }
        drop(state);
        self.emit_best_effort(
            job_id,
            "lease_acquired",
            &format!("{}_paused", component),
            &format!("{component} worker acknowledged a pause lease"),
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
        Ok(())
    }

    fn finish_failed_before_mutation(
        self: &Arc<Self>,
        job_id: &str,
        error: anyhow::Error,
        leases: Vec<JobLease>,
        abort: Arc<AtomicBool>,
    ) {
        let cleanup = self.cleanup_leases(job_id, &leases, false, None);
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
                message: "job executor panicked; worker leases are being recovered conservatively"
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
                        "interrupted job leases are clear",
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
                                job.detail.summary.phase = "recovering_worker_leases".to_string();
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
                            "recovering_worker_leases",
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
        let (kind, detail_request, context) = {
            let state = self.state.lock().expect("job manager lock");
            let job = find_stored(&state.persisted, job_id)
                .ok_or_else(|| anyhow::anyhow!("recovery job {job_id} is missing"))?;
            (
                job.detail.summary.kind,
                job.detail.request.clone(),
                job.reorg_recovery.clone(),
            )
        };
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

    fn ensure_recovery_leases(&self, job_id: &str) -> anyhow::Result<()> {
        let nonce = now_ms();
        let spam_status = self.spam.status()?;
        let spam_leases = owned_leases(&spam_status.active_leases, job_id);
        if spam_leases.is_empty() {
            let lease_id = format!("{job_id}-spam");
            self.spam.acquire_lease(LeaseRequest {
                lease_id: lease_id.clone(),
                owner_job_id: job_id.to_string(),
                purpose: "interrupted reorg recovery".to_string(),
                ttl_secs: self.lease_ttl_secs,
                request_id: format!("{job_id}-spam-recovery-acquire-{nonce}"),
            })?;
            self.record_lease(
                job_id,
                JobLease {
                    component: "spam".to_string(),
                    lease_id,
                    purpose: "interrupted reorg recovery".to_string(),
                },
            );
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
            self.mining.acquire_lease(LeaseRequest {
                lease_id: lease_id.clone(),
                owner_job_id: job_id.to_string(),
                purpose: "interrupted reorg recovery".to_string(),
                ttl_secs: self.lease_ttl_secs,
                request_id: format!("{job_id}-mining-recovery-acquire-{nonce}"),
            })?;
            self.record_lease(
                job_id,
                JobLease {
                    component: "mining".to_string(),
                    lease_id,
                    purpose: "interrupted reorg recovery".to_string(),
                },
            );
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
    mining_paused_by_step: bool,
    chain_changed: bool,
}

struct JobScenarioActions {
    manager: Arc<JobManager>,
    job_id: String,
    abort: Arc<AtomicBool>,
    use_raw_tx_spam: bool,
    runtime: Mutex<ScenarioRuntime>,
}

impl JobScenarioActions {
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
        _control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        let acquired_spam = self.ensure_spam_lease("scenario reorg step")?;
        let acquired_mining = self.ensure_mining_lease("scenario reorg step")?;
        let request = ReorgJobRequest {
            depth,
            empty,
            ..ReorgJobRequest::default()
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

    fn run_partition(
        &self,
        node: MinerNode,
        main_blocks: u64,
        isolated_blocks: u64,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        self.manager
            .scenario
            .run_partition(node, main_blocks, isolated_blocks, control)
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
        let backend = Arc::new(MockBackend::new(dir.join(".env")));
        backend.sync_containers();
        let manager = JobManager::open_with_ttl(
            dir,
            backend.clone(),
            backend.clone(),
            executor,
            backend.clone(),
            60,
        )
        .expect("job manager");
        (backend, manager)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(std::time::Instant::now() < deadline, "condition timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn one_mutation_idempotency_and_event_cursors_are_pinned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor.clone());
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
        assert!(backend.compose_calls().is_empty());
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
        let backend = Arc::new(MockBackend::new(dir.path().join(".env")));
        backend.sync_containers();
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
                    reorg_recovery: ReorgRecoveryContext {
                        mutation_may_have_occurred: true,
                        request: Some(ReorgJobRequest::default()),
                        invalidated_block_hash: Some("00".repeat(32)),
                    },
                }],
            })
            .expect("seed active job");

        let manager = JobManager::open_with_ttl(
            dir.path(),
            backend.clone(),
            backend.clone(),
            Arc::new(BlockingExecutor::new()),
            backend,
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
        let backend = Arc::new(MockBackend::new(dir.path().join(".env")));
        backend.sync_containers();
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
        let manager = JobManager::open_with_ttl(
            dir.path(),
            backend.clone(),
            backend.clone(),
            executor.clone(),
            backend.clone(),
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
        let backend = Arc::new(MockBackend::new(dir.path().join(".env")));
        backend.sync_containers();
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
                    reorg_recovery: ReorgRecoveryContext::default(),
                }],
            })
            .expect("seed held scenario");

        let manager = JobManager::open_with_ttl(
            dir.path(),
            backend.clone(),
            backend.clone(),
            Arc::new(BlockingExecutor::new()),
            backend.clone(),
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
    fn existing_v1_scenarios_run_without_compose_actions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = Arc::new(BlockingExecutor::new());
        let (backend, manager) = manager(dir.path(), executor);
        let created = manager
            .start_scenario(
                r#"
version: 1
steps:
  - type: wait_height
    height: 204
  - type: pause_mining
  - type: mine
    node: btc-simnet-node2
    blocks: 1
  - type: spam_burst
    node: btc-simnet-node2
    txs: 2
    outputs_per_tx: 1
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
        assert!(backend.compose_calls().is_empty());
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
        assert!(backend.compose_calls().is_empty());
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
