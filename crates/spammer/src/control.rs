//! Spammer-worker control state and cooperative safe-point coordination.

use simchain_common::internal_api::{
    CommandAck, DesiredState, LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest, PauseLease,
    SetSpamPolicyRequest, SetStateRequest, SpamCapacityStatus, SpamWorkerStatus, WorkerPhase,
};
use simchain_common::live_tuning::SpamTuning;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SAFE_POINT_TIMEOUT: Duration = Duration::from_secs(30);
const COMPLETED_REQUEST_LIMIT: usize = 1_024;

#[derive(Clone, PartialEq)]
struct LeaseEntry {
    view: PauseLease,
    deadline: Instant,
}

struct PendingPolicy {
    generation: u64,
    policy: SpamTuning,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpamPolicyImpact {
    pub lifecycle_changed: bool,
    pub shape_changed: bool,
    pub capacity_changed: bool,
    pub workload_changed: bool,
    pub engine_changed: bool,
    pub needs_immediate_cycle: bool,
}

struct PolicyFailure {
    generation: u64,
    policy: SpamTuning,
    message: String,
}

#[derive(Clone)]
struct CompletedRequest {
    fingerprint: String,
    ack: CommandAck,
}

struct Inner {
    desired_state: DesiredState,
    leases: HashMap<String, LeaseEntry>,
    policy: SpamTuning,
    generation: u64,
    pending_policy: Option<PendingPolicy>,
    policy_failure: Option<PolicyFailure>,
    initialization_pending: bool,
    reconciliation_pending: bool,
    reconciliation_reason: Option<String>,
    reconciliation_count: u64,
    last_reconciliation_reason: Option<String>,
    capacity: Option<SpamCapacityStatus>,
    phase: WorkerPhase,
    observed_height: Option<u64>,
    cycle_phase: Option<String>,
    accepted_transactions: u64,
    last_cycle_duration_ms: Option<u64>,
    in_flight: bool,
    last_error: Option<String>,
    started: Instant,
    completed_requests: HashMap<String, CompletedRequest>,
    completed_request_order: VecDeque<String>,
}

pub struct SpamControl {
    inner: Mutex<Inner>,
    changed: Condvar,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SafePointAction {
    Initialize {
        generation: u64,
        policy: SpamTuning,
    },
    ApplyPolicy {
        generation: u64,
        policy: SpamTuning,
        impact: SpamPolicyImpact,
    },
    Reconcile,
    Ready {
        generation: u64,
        policy: SpamTuning,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerWait {
    Ready,
    Interrupted,
}

impl SpamControl {
    pub fn new(initial_policy: SpamTuning) -> Arc<Self> {
        let enabled = initial_policy.enabled;
        Arc::new(Self {
            inner: Mutex::new(Inner {
                desired_state: DesiredState::Running,
                leases: HashMap::new(),
                policy: initial_policy,
                generation: 0,
                pending_policy: None,
                policy_failure: None,
                initialization_pending: enabled,
                reconciliation_pending: false,
                reconciliation_reason: None,
                reconciliation_count: 0,
                last_reconciliation_reason: None,
                capacity: None,
                phase: if enabled {
                    WorkerPhase::Initializing
                } else {
                    WorkerPhase::Disabled
                },
                observed_height: None,
                cycle_phase: None,
                accepted_transactions: 0,
                last_cycle_duration_ms: None,
                in_flight: false,
                last_error: None,
                started: Instant::now(),
                completed_requests: HashMap::new(),
                completed_request_order: VecDeque::new(),
            }),
            changed: Condvar::new(),
        })
    }

    pub fn status(&self) -> SpamWorkerStatus {
        let mut inner = self.inner.lock().expect("spam control lock");
        expire_leases(&mut inner);
        status_from(&inner)
    }

    pub fn set_state(&self, request: SetStateRequest) -> anyhow::Result<CommandAck> {
        validate_request_id(&request.request_id)?;
        let fingerprint = request_fingerprint("state", &request)?;
        let mut inner = self.inner.lock().expect("spam control lock");
        if let Some(ack) = completed_request(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        inner.desired_state = request.state;
        if request.state == DesiredState::Paused {
            mark_pause_requested(&mut inner);
        }
        self.changed.notify_all();
        if request.state == DesiredState::Paused {
            inner = self.wait_for_safe_pause(inner, SAFE_POINT_TIMEOUT)?;
        }
        let ack = ack(&inner, request.request_id.clone());
        remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    pub fn acquire_lease(&self, request: LeaseRequest) -> anyhow::Result<CommandAck> {
        self.acquire_lease_with_timeout(request, SAFE_POINT_TIMEOUT)
    }

    fn acquire_lease_with_timeout(
        &self,
        request: LeaseRequest,
        timeout: Duration,
    ) -> anyhow::Result<CommandAck> {
        validate_lease_request(&request)?;
        let fingerprint = request_fingerprint("lease-acquire", &request)?;
        let mut inner = self.inner.lock().expect("spam control lock");
        expire_leases(&mut inner);
        if let Some(ack) = completed_request(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        if let Some(existing) = inner.leases.get(&request.lease_id) {
            if existing.view.owner_job_id != request.owner_job_id
                || existing.view.purpose != request.purpose
            {
                anyhow::bail!("lease ID is already owned by a different request");
            }
        }
        let lease_id = request.lease_id.clone();
        let expires_at_ms = now_ms().saturating_add(request.ttl_secs.saturating_mul(1_000));
        let tentative_lease = LeaseEntry {
            view: PauseLease {
                lease_id: request.lease_id,
                owner_job_id: request.owner_job_id,
                purpose: request.purpose,
                expires_at_ms,
            },
            deadline: Instant::now() + Duration::from_secs(request.ttl_secs),
        };
        let previous_lease = inner
            .leases
            .insert(lease_id.clone(), tentative_lease.clone());
        mark_pause_requested(&mut inner);
        self.changed.notify_all();
        inner = match self.wait_for_safe_pause(inner, timeout) {
            Ok(inner) => inner,
            Err(error) => {
                self.rollback_unacquired_lease(&lease_id, &tentative_lease, previous_lease);
                return Err(error);
            }
        };
        let ack = ack(&inner, request.request_id.clone());
        remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    pub fn renew_lease(
        &self,
        lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<CommandAck> {
        validate_request_id(&request.request_id)?;
        if request.ttl_secs == 0 {
            anyhow::bail!("lease ttl_secs must be positive");
        }
        let fingerprint = request_fingerprint(&format!("lease-renew:{lease_id}"), &request)?;
        let mut inner = self.inner.lock().expect("spam control lock");
        expire_leases(&mut inner);
        if let Some(ack) = completed_request(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        let lease = inner
            .leases
            .get_mut(lease_id)
            .ok_or_else(|| anyhow::anyhow!("lease not found"))?;
        lease.deadline = Instant::now() + Duration::from_secs(request.ttl_secs);
        lease.view.expires_at_ms = now_ms().saturating_add(request.ttl_secs.saturating_mul(1_000));
        let ack = ack(&inner, request.request_id.clone());
        remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    pub fn release_lease(
        &self,
        lease_id: &str,
        request: LeaseReleaseRequest,
    ) -> anyhow::Result<CommandAck> {
        validate_request_id(&request.request_id)?;
        let fingerprint = request_fingerprint(&format!("lease-release:{lease_id}"), &request)?;
        let mut inner = self.inner.lock().expect("spam control lock");
        expire_leases(&mut inner);
        if let Some(ack) = completed_request(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        inner.leases.remove(lease_id);
        if request.chain_changed {
            inner.reconciliation_pending = true;
            inner.reconciliation_reason = Some(format!("chain changed under lease {lease_id}"));
            tracing::info!(lease_id, "spam pause lease released after a chain change");
        }
        self.changed.notify_all();
        let ack = ack(&inner, request.request_id.clone());
        remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    pub fn set_policy(&self, request: SetSpamPolicyRequest) -> anyhow::Result<CommandAck> {
        validate_request_id(&request.request_id)?;
        validate_policy(&request.policy)?;
        let fingerprint = request_fingerprint("policy", &request)?;
        let mut inner = self.inner.lock().expect("spam control lock");
        expire_leases(&mut inner);
        if let Some(ack) = completed_request(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        if !inner.leases.is_empty() {
            anyhow::bail!("spam policy cannot change while a pause lease is active");
        }
        if request.generation < inner.generation && !request.rollback {
            anyhow::bail!("policy generation is stale");
        }
        if request.generation == inner.generation && request.policy == inner.policy {
            let ack = ack(&inner, request.request_id.clone());
            remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
            return Ok(ack);
        }
        if request.generation == inner.generation && !request.rollback {
            anyhow::bail!("generation already identifies a different policy");
        }
        if let Some(pending) = &inner.pending_policy {
            if pending.generation == request.generation && pending.policy == request.policy {
                inner = self.wait_for_policy(inner, request.generation, &request.policy)?;
                let ack = ack(&inner, request.request_id.clone());
                remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
                return Ok(ack);
            }
            anyhow::bail!("another spam policy generation is pending");
        }
        inner.policy_failure = None;
        inner.pending_policy = Some(PendingPolicy {
            generation: request.generation,
            policy: request.policy.clone(),
        });
        self.changed.notify_all();
        inner = self.wait_for_policy(inner, request.generation, &request.policy)?;
        let ack = ack(&inner, request.request_id.clone());
        remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    /// Blocks at a worker boundary until the worker has a concrete action.
    pub fn safe_point(&self) -> SafePointAction {
        let mut inner = self.inner.lock().expect("spam control lock");
        debug_assert!(!inner.in_flight);
        loop {
            expire_leases(&mut inner);
            if let Some(pending) = &inner.pending_policy {
                let impact = policy_impact(&inner.policy, &pending.policy);
                let action = SafePointAction::ApplyPolicy {
                    generation: pending.generation,
                    policy: pending.policy.clone(),
                    impact,
                };
                inner.in_flight = true;
                inner.phase = WorkerPhase::Active;
                inner.cycle_phase = Some("applying_policy".to_string());
                self.changed.notify_all();
                return action;
            }
            if pause_requested(&inner) {
                inner.phase = WorkerPhase::Paused;
                inner.cycle_phase = None;
                self.changed.notify_all();
                inner = self.wait_for_change_or_expiry(inner);
                continue;
            }
            if inner.initialization_pending {
                inner.in_flight = true;
                inner.phase = WorkerPhase::Initializing;
                inner.cycle_phase = Some("initializing_engine".to_string());
                self.changed.notify_all();
                return SafePointAction::Initialize {
                    generation: inner.generation,
                    policy: inner.policy.clone(),
                };
            }
            if inner.reconciliation_pending && inner.policy.enabled {
                inner.in_flight = true;
                inner.phase = WorkerPhase::Reconciling;
                inner.cycle_phase = Some("reconciling_engine".to_string());
                self.changed.notify_all();
                return SafePointAction::Reconcile;
            }
            if !inner.policy.enabled {
                inner.phase = WorkerPhase::Disabled;
                inner.cycle_phase = None;
                self.changed.notify_all();
                inner = self.wait_for_change_or_expiry(inner);
                continue;
            }
            inner.phase = WorkerPhase::Active;
            inner.cycle_phase = Some("waiting_for_block".to_string());
            self.changed.notify_all();
            return SafePointAction::Ready {
                generation: inner.generation,
                policy: inner.policy.clone(),
            };
        }
    }

    pub fn complete_initialization(&self, result: anyhow::Result<()>) {
        let mut inner = self.inner.lock().expect("spam control lock");
        inner.in_flight = false;
        inner.cycle_phase = None;
        match result {
            Ok(()) => {
                inner.initialization_pending = false;
                inner.last_error = None;
                inner.phase = if pause_requested(&inner) {
                    WorkerPhase::Paused
                } else {
                    WorkerPhase::Active
                };
            }
            Err(error) => {
                inner.last_error = Some(error.to_string());
                inner.phase = WorkerPhase::Error;
            }
        }
        self.changed.notify_all();
    }

    pub fn record_recovery(&self, reason: impl Into<String>) {
        let mut inner = self.inner.lock().expect("spam control lock");
        inner.reconciliation_count = inner.reconciliation_count.saturating_add(1);
        inner.last_reconciliation_reason = Some(reason.into());
        self.changed.notify_all();
    }

    pub fn report_capacity(&self, capacity: SpamCapacityStatus) {
        let mut inner = self.inner.lock().expect("spam control lock");
        inner.capacity = Some(capacity);
        self.changed.notify_all();
    }

    pub fn complete_policy(&self, result: anyhow::Result<()>, engine_available: bool) {
        let mut inner = self.inner.lock().expect("spam control lock");
        let pending = inner
            .pending_policy
            .take()
            .expect("complete_policy requires a pending policy");
        inner.in_flight = false;
        inner.cycle_phase = None;
        match result {
            Ok(()) => {
                inner.policy = pending.policy;
                inner.generation = pending.generation;
                inner.initialization_pending = false;
                inner.policy_failure = None;
                inner.last_error = None;
                inner.phase = if pause_requested(&inner) {
                    WorkerPhase::Paused
                } else if inner.policy.enabled {
                    WorkerPhase::Active
                } else {
                    WorkerPhase::Disabled
                };
            }
            Err(error) => {
                let message = error.to_string();
                inner.initialization_pending = inner.policy.enabled && !engine_available;
                inner.policy_failure = Some(PolicyFailure {
                    generation: pending.generation,
                    policy: pending.policy,
                    message: message.clone(),
                });
                inner.last_error = Some(message);
                inner.phase = WorkerPhase::Error;
            }
        }
        self.changed.notify_all();
    }

    pub fn complete_reconciliation(&self, result: anyhow::Result<()>) {
        let mut inner = self.inner.lock().expect("spam control lock");
        inner.in_flight = false;
        inner.cycle_phase = None;
        match result {
            Ok(()) => {
                inner.reconciliation_pending = false;
                inner.reconciliation_count = inner.reconciliation_count.saturating_add(1);
                inner.last_reconciliation_reason = inner.reconciliation_reason.take();
                inner.last_error = None;
                inner.phase = if pause_requested(&inner) {
                    WorkerPhase::Paused
                } else {
                    WorkerPhase::Active
                };
            }
            Err(error) => {
                inner.last_error = Some(error.to_string());
                inner.phase = WorkerPhase::Error;
            }
        }
        self.changed.notify_all();
    }

    /// Atomically crosses the safe point into a spam cycle.
    pub fn begin_cycle(&self, generation: u64, height: u64) -> bool {
        let mut inner = self.inner.lock().expect("spam control lock");
        expire_leases(&mut inner);
        if pause_requested(&inner)
            || inner.pending_policy.is_some()
            || inner.reconciliation_pending
            || inner.initialization_pending
            || inner.generation != generation
            || !inner.policy.enabled
        {
            self.changed.notify_all();
            return false;
        }
        inner.in_flight = true;
        inner.observed_height = Some(height);
        inner.phase = WorkerPhase::Active;
        inner.cycle_phase = Some("cycle_start".to_string());
        self.changed.notify_all();
        true
    }

    /// A cooperative boundary before/after a submitted unit of spam work.
    pub fn cycle_checkpoint(&self, generation: u64, phase: &str) -> bool {
        let mut inner = self.inner.lock().expect("spam control lock");
        expire_leases(&mut inner);
        inner.cycle_phase = Some(phase.to_string());
        let interrupted = pause_requested(&inner)
            || inner.pending_policy.is_some()
            || inner.reconciliation_pending
            || inner.generation != generation
            || !inner.policy.enabled;
        if interrupted {
            inner.phase = if pause_requested(&inner) {
                WorkerPhase::Pausing
            } else if inner.reconciliation_pending {
                WorkerPhase::Reconciling
            } else {
                WorkerPhase::Active
            };
        }
        self.changed.notify_all();
        !interrupted
    }

    pub fn finish_cycle(&self, height: u64, accepted: usize, duration: Duration) {
        let mut inner = self.inner.lock().expect("spam control lock");
        inner.in_flight = false;
        inner.observed_height = Some(height);
        inner.accepted_transactions = inner.accepted_transactions.saturating_add(accepted as u64);
        inner.last_cycle_duration_ms = Some(duration.as_millis().min(u64::MAX as u128) as u64);
        inner.cycle_phase = None;
        inner.phase = if pause_requested(&inner) {
            WorkerPhase::Paused
        } else if inner.reconciliation_pending {
            WorkerPhase::Reconciling
        } else if inner.pending_policy.is_some() || inner.policy.enabled {
            WorkerPhase::Active
        } else {
            WorkerPhase::Disabled
        };
        self.changed.notify_all();
    }

    pub fn record_error(&self, message: String) {
        let mut inner = self.inner.lock().expect("spam control lock");
        inner.last_error = Some(message);
    }

    pub fn wait_for_block_poll(&self, duration: Duration, generation: u64) -> WorkerWait {
        let mut inner = self.inner.lock().expect("spam control lock");
        expire_leases(&mut inner);
        if work_interrupted(&inner, generation) {
            return WorkerWait::Interrupted;
        }
        let wait = duration.min(time_until_lease_expiry(&inner));
        let (next, _) = self
            .changed
            .wait_timeout(inner, wait)
            .expect("spam control wait");
        inner = next;
        expire_leases(&mut inner);
        if work_interrupted(&inner, generation) {
            WorkerWait::Interrupted
        } else {
            WorkerWait::Ready
        }
    }

    fn wait_for_safe_pause<'a>(
        &self,
        mut inner: MutexGuard<'a, Inner>,
        timeout: Duration,
    ) -> anyhow::Result<MutexGuard<'a, Inner>> {
        let deadline = Instant::now() + timeout;
        while !is_effectively_paused(&inner) {
            let now = Instant::now();
            if now >= deadline {
                anyhow::bail!("timed out waiting for spam safe point");
            }
            let (next, result) = self
                .changed
                .wait_timeout(inner, deadline - now)
                .expect("spam control wait");
            inner = next;
            if result.timed_out() && !is_effectively_paused(&inner) {
                anyhow::bail!("timed out waiting for spam safe point");
            }
        }
        Ok(inner)
    }

    fn wait_for_policy<'a>(
        &self,
        mut inner: MutexGuard<'a, Inner>,
        generation: u64,
        policy: &SpamTuning,
    ) -> anyhow::Result<MutexGuard<'a, Inner>> {
        let deadline = Instant::now() + SAFE_POINT_TIMEOUT;
        loop {
            if inner.generation == generation && &inner.policy == policy {
                return Ok(inner);
            }
            if let Some(failure) = &inner.policy_failure {
                if failure.generation == generation && &failure.policy == policy {
                    anyhow::bail!("spam policy rejected: {}", failure.message);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                anyhow::bail!("timed out waiting for spam policy safe point");
            }
            let (next, result) = self
                .changed
                .wait_timeout(inner, deadline - now)
                .expect("spam control wait");
            inner = next;
            if result.timed_out() {
                anyhow::bail!("timed out waiting for spam policy safe point");
            }
        }
    }

    fn wait_for_change_or_expiry<'a>(&self, inner: MutexGuard<'a, Inner>) -> MutexGuard<'a, Inner> {
        let wait = time_until_lease_expiry(&inner);
        self.changed
            .wait_timeout(inner, wait)
            .expect("spam control wait")
            .0
    }

    fn rollback_unacquired_lease(
        &self,
        lease_id: &str,
        tentative: &LeaseEntry,
        previous: Option<LeaseEntry>,
    ) {
        let mut inner = self.inner.lock().expect("spam control lock");
        if inner.leases.get(lease_id) == Some(tentative) {
            match previous {
                Some(lease) => {
                    inner.leases.insert(lease_id.to_string(), lease);
                }
                None => {
                    inner.leases.remove(lease_id);
                }
            }
            expire_leases(&mut inner);
            self.changed.notify_all();
        }
    }
}

pub fn policy_impact(current: &SpamTuning, next: &SpamTuning) -> SpamPolicyImpact {
    let lifecycle_changed = current.enabled != next.enabled;
    let engine_changed = current.use_raw != next.use_raw;
    let shape_changed = engine_changed
        || current.spam_fee != next.spam_fee
        || current.sendmany_outputs != next.sendmany_outputs
        || current.data_min_bytes != next.data_min_bytes
        || current.data_max_bytes != next.data_max_bytes;
    let capacity_changed = shape_changed
        || current.fill_block_ratio != next.fill_block_ratio
        || current.fanout_auto != next.fanout_auto
        || current.fanout_utxos != next.fanout_utxos;
    let workload_changed = current.fixed_txs_per_block != next.fixed_txs_per_block
        || current.small_txs_per_block != next.small_txs_per_block
        || current.floor_pool_txs != next.floor_pool_txs
        || current.fill_block_ratio != next.fill_block_ratio
        || current.enable_replaces != next.enable_replaces
        || current.replaces_per_miner != next.replaces_per_miner;
    let workload_increased = next.fill_block_ratio > current.fill_block_ratio
        || next.fixed_txs_per_block > current.fixed_txs_per_block
        || next.small_txs_per_block > current.small_txs_per_block
        || next.floor_pool_txs > current.floor_pool_txs;
    // Wake immediately for a capacity-only increase so asynchronous funding or
    // fanout starts now instead of waiting for the next block. Catch-up policy
    // suppresses unchanged additive transaction counts.
    let capacity_increased = next.desired_data_fanout() > current.desired_data_fanout();

    SpamPolicyImpact {
        lifecycle_changed,
        shape_changed,
        capacity_changed,
        workload_changed,
        engine_changed,
        needs_immediate_cycle: next.enabled
            && ((!current.enabled && lifecycle_changed)
                || shape_changed
                || workload_increased
                || capacity_increased),
    }
}

fn validate_lease_request(request: &LeaseRequest) -> anyhow::Result<()> {
    if request.lease_id.trim().is_empty()
        || request.owner_job_id.trim().is_empty()
        || request.purpose.trim().is_empty()
        || request.request_id.trim().is_empty()
    {
        anyhow::bail!("lease identifiers, owner, purpose, and request ID must be non-empty");
    }
    if request.ttl_secs == 0 {
        anyhow::bail!("lease ttl_secs must be positive");
    }
    Ok(())
}

fn validate_request_id(request_id: &str) -> anyhow::Result<()> {
    if request_id.trim().is_empty() {
        anyhow::bail!("request ID must be non-empty");
    }
    Ok(())
}

fn request_fingerprint(operation: &str, request: &impl serde::Serialize) -> anyhow::Result<String> {
    Ok(format!("{operation}:{}", serde_json::to_string(request)?))
}

fn completed_request(
    inner: &Inner,
    request_id: &str,
    fingerprint: &str,
) -> anyhow::Result<Option<CommandAck>> {
    let Some(completed) = inner.completed_requests.get(request_id) else {
        return Ok(None);
    };
    if completed.fingerprint != fingerprint {
        anyhow::bail!("request ID was already used for a different command");
    }
    Ok(Some(completed.ack.clone()))
}

fn remember_request(inner: &mut Inner, request_id: String, fingerprint: String, ack: CommandAck) {
    if inner.completed_requests.contains_key(&request_id) {
        return;
    }
    inner.completed_request_order.push_back(request_id.clone());
    inner
        .completed_requests
        .insert(request_id, CompletedRequest { fingerprint, ack });
    while inner.completed_request_order.len() > COMPLETED_REQUEST_LIMIT {
        if let Some(expired) = inner.completed_request_order.pop_front() {
            inner.completed_requests.remove(&expired);
        }
    }
}

fn validate_policy(policy: &SpamTuning) -> anyhow::Result<()> {
    let source: BTreeMap<String, String> = policy
        .canonical_values()
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    let (reparsed, _) = SpamTuning::from_source(&source)?;
    if &reparsed != policy {
        anyhow::bail!("spam policy is not canonical");
    }
    Ok(())
}

fn pause_requested(inner: &Inner) -> bool {
    inner.desired_state == DesiredState::Paused || !inner.leases.is_empty()
}

fn mark_pause_requested(inner: &mut Inner) {
    inner.phase = if inner.in_flight {
        WorkerPhase::Pausing
    } else {
        WorkerPhase::Paused
    };
    if !inner.in_flight {
        inner.cycle_phase = None;
    }
}

fn is_effectively_paused(inner: &Inner) -> bool {
    inner.phase == WorkerPhase::Paused && !inner.in_flight
}

fn work_interrupted(inner: &Inner, generation: u64) -> bool {
    pause_requested(inner)
        || inner.pending_policy.is_some()
        || inner.reconciliation_pending
        || inner.initialization_pending
        || inner.generation != generation
        || !inner.policy.enabled
}

fn expire_leases(inner: &mut Inner) {
    let now = Instant::now();
    let previous = inner.leases.len();
    inner.leases.retain(|_, lease| lease.deadline > now);
    if inner.leases.len() != previous {
        // An expired owner cannot tell us whether it changed the chain. The
        // conservative recovery path is to reconcile before sending again.
        inner.reconciliation_pending = true;
        inner.reconciliation_reason = Some("pause lease expired".to_string());
    }
}

fn time_until_lease_expiry(inner: &Inner) -> Duration {
    let now = Instant::now();
    inner
        .leases
        .values()
        .map(|lease| lease.deadline.saturating_duration_since(now))
        .min()
        .unwrap_or(Duration::from_secs(60))
        .max(Duration::from_millis(1))
}

fn status_from(inner: &Inner) -> SpamWorkerStatus {
    let mut active_leases: Vec<PauseLease> = inner
        .leases
        .values()
        .map(|lease| lease.view.clone())
        .collect();
    active_leases.sort_by(|left, right| left.lease_id.cmp(&right.lease_id));
    SpamWorkerStatus {
        component: "spam".to_string(),
        phase: inner.phase,
        desired_state: inner.desired_state,
        effective_state: if is_effectively_paused(inner) {
            DesiredState::Paused
        } else {
            DesiredState::Running
        },
        policy: inner.policy.clone(),
        effective_generation: inner.generation,
        observed_height: inner.observed_height,
        cycle_phase: inner.cycle_phase.clone(),
        accepted_transactions: inner.accepted_transactions,
        last_cycle_duration_ms: inner.last_cycle_duration_ms,
        active_leases,
        reconciliation_pending: inner.reconciliation_pending,
        capacity: inner.capacity.clone(),
        reconciliation_count: inner.reconciliation_count,
        last_reconciliation_reason: inner.last_reconciliation_reason.clone(),
        uptime_secs: inner.started.elapsed().as_secs(),
        last_error: inner.last_error.clone(),
    }
}

fn ack(inner: &Inner, request_id: String) -> CommandAck {
    CommandAck {
        request_id,
        phase: inner.phase,
        effective_generation: inner.generation,
    }
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
    use simchain_common::live_tuning;

    fn policy() -> SpamTuning {
        SpamTuning::from_source(&live_tuning::staged_map(&BTreeMap::new()))
            .expect("default spam policy")
            .0
    }

    fn initialize(control: &SpamControl) {
        assert!(matches!(
            control.safe_point(),
            SafePointAction::Initialize { .. }
        ));
        control.complete_initialization(Ok(()));
    }

    #[test]
    fn disabled_policy_is_a_resident_phase() {
        let mut initial = policy();
        initial.enabled = false;
        let control = SpamControl::new(initial);
        let status = control.status();
        assert_eq!(status.phase, WorkerPhase::Disabled);
        assert_eq!(status.effective_state, DesiredState::Running);
    }

    #[test]
    fn successful_initialization_does_not_count_as_recovery() {
        let control = SpamControl::new(policy());
        initialize(&control);

        let status = control.status();
        assert_eq!(status.reconciliation_count, 0);
        assert_eq!(status.last_reconciliation_reason, None);
    }

    #[test]
    fn reconstructed_engine_counts_as_recovery() {
        let control = SpamControl::new(policy());
        initialize(&control);

        control.record_recovery("engine reconstructed after loss");

        let status = control.status();
        assert_eq!(status.reconciliation_count, 1);
        assert_eq!(
            status.last_reconciliation_reason.as_deref(),
            Some("engine reconstructed after loss")
        );
    }

    #[test]
    fn pause_is_acknowledged_after_the_in_flight_cycle_stops() {
        let control = SpamControl::new(policy());
        initialize(&control);
        assert!(control.begin_cycle(0, 100));
        let setter = control.clone();
        let pause = std::thread::spawn(move || {
            setter.set_state(SetStateRequest {
                state: DesiredState::Paused,
                request_id: "pause-cycle".to_string(),
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(!control.cycle_checkpoint(0, "after_transaction"));
        control.finish_cycle(100, 1, Duration::from_millis(42));
        assert_eq!(control.status().last_cycle_duration_ms, Some(42));
        let ack = pause.join().expect("pause thread").expect("pause");
        assert_eq!(ack.phase, WorkerPhase::Paused);
    }

    #[test]
    fn hot_policy_applies_without_an_engine_rebuild() {
        let control = SpamControl::new(policy());
        initialize(&control);
        let mut changed = policy();
        changed.fill_block_ratio = 3.0;
        let setter = control.clone();
        let for_setter = changed.clone();
        let apply = std::thread::spawn(move || {
            setter.set_policy(SetSpamPolicyRequest {
                generation: 1,
                policy: for_setter,
                request_id: "hot-policy".to_string(),
                rollback: false,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            control.safe_point(),
            SafePointAction::ApplyPolicy {
                generation: 1,
                policy: changed.clone(),
                impact: SpamPolicyImpact {
                    capacity_changed: true,
                    workload_changed: true,
                    needs_immediate_cycle: true,
                    ..SpamPolicyImpact::default()
                },
            }
        );
        control.complete_policy(Ok(()), true);
        apply.join().expect("apply thread").expect("apply");
        let status = control.status();
        assert_eq!(status.policy, changed);
        assert_eq!(status.reconciliation_count, 0);
        assert_eq!(status.last_reconciliation_reason, None);
    }

    #[test]
    fn manual_pause_is_a_valid_policy_safe_point() {
        let control = SpamControl::new(policy());
        initialize(&control);
        control
            .set_state(SetStateRequest {
                state: DesiredState::Paused,
                request_id: "manual-pause".to_string(),
            })
            .expect("pause");
        let mut changed = policy();
        changed.fill_block_ratio = 3.0;
        let setter = control.clone();
        let for_setter = changed.clone();
        let apply = std::thread::spawn(move || {
            setter.set_policy(SetSpamPolicyRequest {
                generation: 1,
                policy: for_setter,
                request_id: "paused-policy".to_string(),
                rollback: false,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            control.safe_point(),
            SafePointAction::ApplyPolicy { generation: 1, .. }
        ));
        control.complete_policy(Ok(()), true);
        let ack = apply.join().expect("apply thread").expect("apply");
        assert_eq!(ack.phase, WorkerPhase::Paused);
        assert_eq!(control.status().effective_state, DesiredState::Paused);
    }

    #[test]
    fn active_lease_rejects_policy_instead_of_racing_mutation() {
        let control = SpamControl::new(policy());
        initialize(&control);
        control
            .acquire_lease(LeaseRequest {
                lease_id: "mutation".to_string(),
                owner_job_id: "job".to_string(),
                purpose: "chain mutation".to_string(),
                ttl_secs: 60,
                request_id: "lease-mutation".to_string(),
            })
            .expect("lease");
        let mut changed = policy();
        changed.fill_block_ratio = 3.0;
        let error = control
            .set_policy(SetSpamPolicyRequest {
                generation: 1,
                policy: changed,
                request_id: "leased-policy".to_string(),
                rollback: false,
            })
            .expect_err("lease must serialize policy");
        assert!(error.to_string().contains("pause lease"));
    }

    #[test]
    fn rejected_hot_apply_keeps_the_previous_policy() {
        let initial = policy();
        let control = SpamControl::new(initial.clone());
        initialize(&control);
        let mut changed = initial.clone();
        changed.spam_fee = 0.0002;
        let setter = control.clone();
        let for_setter = changed.clone();
        let apply = std::thread::spawn(move || {
            setter.set_policy(SetSpamPolicyRequest {
                generation: 2,
                policy: for_setter,
                request_id: "bad-rebuild".to_string(),
                rollback: false,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            control.safe_point(),
            SafePointAction::ApplyPolicy {
                impact: SpamPolicyImpact {
                    shape_changed: true,
                    engine_changed: false,
                    ..
                },
                ..
            }
        ));
        control.complete_policy(Err(anyhow::anyhow!("settxfee rejected")), true);
        let error = apply
            .join()
            .expect("apply thread")
            .expect_err("rebuild must fail");
        assert!(error.to_string().contains("settxfee rejected"));
        assert_eq!(control.status().policy, initial);
        assert_eq!(control.status().effective_generation, 0);
    }

    #[test]
    fn failed_apply_without_an_engine_reinitializes_the_previous_policy() {
        let initial = policy();
        let control = SpamControl::new(initial.clone());
        initialize(&control);
        let mut changed = initial.clone();
        changed.spam_fee = 0.0002;
        let setter = control.clone();
        let apply = std::thread::spawn(move || {
            setter.set_policy(SetSpamPolicyRequest {
                generation: 1,
                policy: changed,
                request_id: "unsafe-rebuild".to_string(),
                rollback: false,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            control.safe_point(),
            SafePointAction::ApplyPolicy {
                impact: SpamPolicyImpact {
                    shape_changed: true,
                    ..
                },
                ..
            }
        ));
        control.complete_policy(Err(anyhow::anyhow!("wallet rollback failed")), false);
        apply
            .join()
            .expect("apply thread")
            .expect_err("rebuild must fail");

        assert_eq!(
            control.safe_point(),
            SafePointAction::Initialize {
                generation: 0,
                policy: initial,
            }
        );
    }

    #[test]
    fn expired_lease_requires_reconciliation_before_resume() {
        let control = SpamControl::new(policy());
        initialize(&control);
        control
            .acquire_lease(LeaseRequest {
                lease_id: "expired".to_string(),
                owner_job_id: "job".to_string(),
                purpose: "reorg".to_string(),
                ttl_secs: 60,
                request_id: "acquire-expired".to_string(),
            })
            .expect("lease");
        {
            let mut inner = control.inner.lock().expect("control lock");
            inner.leases.get_mut("expired").expect("lease").deadline = Instant::now();
            control.changed.notify_all();
        }
        assert_eq!(control.safe_point(), SafePointAction::Reconcile);
        assert!(control.status().reconciliation_pending);
        control.complete_reconciliation(Ok(()));
        let status = control.status();
        assert_eq!(status.reconciliation_count, 1);
        assert_eq!(
            status.last_reconciliation_reason.as_deref(),
            Some("pause lease expired")
        );
        assert!(matches!(
            control.safe_point(),
            SafePointAction::Ready { .. }
        ));
    }

    #[test]
    fn policy_impact_classifies_shape_lifecycle_and_workload_changes() {
        let initial = policy();
        let mut shape = initial.clone();
        shape.data_max_bytes = 0;
        shape.data_min_bytes = 0;
        let impact = policy_impact(&initial, &shape);
        assert!(impact.shape_changed);
        assert!(impact.capacity_changed);
        assert!(impact.needs_immediate_cycle);
        assert!(!impact.engine_changed);

        let mut disabled = initial.clone();
        disabled.enabled = false;
        let impact = policy_impact(&disabled, &initial);
        assert!(impact.lifecycle_changed);
        assert!(impact.needs_immediate_cycle);
        assert!(!impact.shape_changed);

        let mut workload = initial.clone();
        workload.fixed_txs_per_block += 1;
        let impact = policy_impact(&initial, &workload);
        assert!(impact.workload_changed);
        assert!(impact.needs_immediate_cycle);
        assert!(!impact.shape_changed);

        let mut reduction = initial.clone();
        reduction.fill_block_ratio /= 2.0;
        let impact = policy_impact(&initial, &reduction);
        assert!(impact.workload_changed);
        assert!(impact.capacity_changed);
        assert!(!impact.needs_immediate_cycle);
    }

    #[test]
    fn every_policy_field_has_an_explicit_impact() {
        let initial = policy();
        type ImpactCase = (&'static str, SpamTuning, fn(SpamPolicyImpact) -> bool);
        let mut cases: Vec<ImpactCase> = Vec::new();

        let mut changed = initial.clone();
        changed.enabled = !changed.enabled;
        cases.push(("enabled", changed, |impact| impact.lifecycle_changed));
        let mut changed = initial.clone();
        changed.use_raw = !changed.use_raw;
        cases.push(("use_raw", changed, |impact| impact.engine_changed));
        let mut changed = initial.clone();
        changed.spam_fee *= 2.0;
        cases.push(("spam_fee", changed, |impact| impact.shape_changed));
        let mut changed = initial.clone();
        changed.fixed_txs_per_block += 1;
        cases.push(("fixed_txs_per_block", changed, |impact| {
            impact.workload_changed
        }));
        let mut changed = initial.clone();
        changed.sendmany_outputs += 1;
        cases.push(("sendmany_outputs", changed, |impact| impact.shape_changed));
        let mut changed = initial.clone();
        changed.data_max_bytes -= 1;
        cases.push(("data_max_bytes", changed, |impact| impact.shape_changed));
        let mut changed = initial.clone();
        changed.data_min_bytes -= 1;
        cases.push(("data_min_bytes", changed, |impact| impact.shape_changed));
        let mut changed = initial.clone();
        changed.small_txs_per_block += 1;
        cases.push(("small_txs_per_block", changed, |impact| {
            impact.workload_changed
        }));
        let mut changed = initial.clone();
        changed.floor_pool_txs += 1;
        cases.push(("floor_pool_txs", changed, |impact| impact.workload_changed));
        let mut changed = initial.clone();
        changed.fill_block_ratio += 1.0;
        cases.push(("fill_block_ratio", changed, |impact| {
            impact.workload_changed && impact.capacity_changed
        }));
        let mut changed = initial.clone();
        changed.fanout_auto = !changed.fanout_auto;
        cases.push(("fanout_auto", changed, |impact| impact.capacity_changed));
        let mut changed = initial.clone();
        changed.fanout_utxos += 1;
        cases.push(("fanout_utxos", changed, |impact| impact.capacity_changed));
        let mut changed = initial.clone();
        changed.enable_replaces = !changed.enable_replaces;
        cases.push(("enable_replaces", changed, |impact| impact.workload_changed));
        let mut changed = initial.clone();
        changed.replaces_per_miner += 1;
        cases.push(("replaces_per_miner", changed, |impact| {
            impact.workload_changed
        }));

        for (name, changed, classified) in cases {
            let impact = policy_impact(&initial, &changed);
            assert!(
                classified(impact),
                "field {name} was misclassified: {impact:?}"
            );
        }
    }

    #[test]
    fn leases_are_idempotent_owned_and_renewable() {
        let control = SpamControl::new(policy());
        initialize(&control);
        let request = LeaseRequest {
            lease_id: "lease-1".to_string(),
            owner_job_id: "job-1".to_string(),
            purpose: "reorg".to_string(),
            ttl_secs: 60,
            request_id: "lease-request".to_string(),
        };
        let first = control.acquire_lease(request.clone()).expect("lease");
        let second = control.acquire_lease(request).expect("idempotent lease");
        assert_eq!(first, second);
        assert_eq!(control.status().active_leases.len(), 1);

        let conflict = control
            .acquire_lease(LeaseRequest {
                lease_id: "lease-1".to_string(),
                owner_job_id: "job-2".to_string(),
                purpose: "other".to_string(),
                ttl_secs: 60,
                request_id: "lease-conflict".to_string(),
            })
            .expect_err("ownership conflict");
        assert!(conflict.to_string().contains("different request"));

        control
            .renew_lease(
                "lease-1",
                LeaseRenewRequest {
                    ttl_secs: 120,
                    request_id: "lease-renew".to_string(),
                },
            )
            .expect("renew");
        control
            .release_lease(
                "lease-1",
                LeaseReleaseRequest {
                    chain_changed: true,
                    request_id: "lease-release".to_string(),
                },
            )
            .expect("release");
        assert_eq!(control.safe_point(), SafePointAction::Reconcile);
    }

    #[test]
    fn failed_lease_acquire_does_not_leave_orphan_pause() {
        let control = SpamControl::new(policy());
        assert!(matches!(
            control.safe_point(),
            SafePointAction::Initialize { .. }
        ));
        let error = control
            .acquire_lease_with_timeout(
                LeaseRequest {
                    lease_id: "timed-out".to_string(),
                    owner_job_id: "job".to_string(),
                    purpose: "test timeout".to_string(),
                    ttl_secs: 60,
                    request_id: "acquire-timeout".to_string(),
                },
                Duration::from_millis(5),
            )
            .expect_err("safe point never arrives");
        assert!(error
            .to_string()
            .contains("timed out waiting for spam safe point"));
        assert!(control.status().active_leases.is_empty());
        assert!(!control.status().reconciliation_pending);
        control.complete_initialization(Ok(()));
    }

    #[test]
    fn disabled_worker_can_reenable_without_process_restart() {
        let mut disabled = policy();
        disabled.enabled = false;
        let control = SpamControl::new(disabled);
        let enabled = policy();
        let setter = control.clone();
        let for_setter = enabled.clone();
        let apply = std::thread::spawn(move || {
            setter.set_policy(SetSpamPolicyRequest {
                generation: 1,
                policy: for_setter,
                request_id: "reenable".to_string(),
                rollback: false,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            control.safe_point(),
            SafePointAction::ApplyPolicy {
                generation: 1,
                policy: enabled,
                impact: SpamPolicyImpact {
                    lifecycle_changed: true,
                    needs_immediate_cycle: true,
                    ..SpamPolicyImpact::default()
                },
            }
        );
        control.complete_policy(Ok(()), true);
        apply.join().expect("apply thread").expect("reenable");
        assert_eq!(control.status().phase, WorkerPhase::Active);
    }

    #[test]
    fn transaction_rollback_can_restore_an_older_generation() {
        let initial = policy();
        let control = SpamControl::new(initial.clone());
        initialize(&control);
        let mut changed = initial.clone();
        changed.fill_block_ratio = 3.0;

        let setter = control.clone();
        let changed_for_setter = changed.clone();
        let apply = std::thread::spawn(move || {
            setter.set_policy(SetSpamPolicyRequest {
                generation: 3,
                policy: changed_for_setter,
                request_id: "generation-3".to_string(),
                rollback: false,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            control.safe_point(),
            SafePointAction::ApplyPolicy { generation: 3, .. }
        ));
        control.complete_policy(Ok(()), true);
        apply.join().expect("apply thread").expect("apply");

        let rollback_control = control.clone();
        let initial_for_rollback = initial.clone();
        let rollback = std::thread::spawn(move || {
            rollback_control.set_policy(SetSpamPolicyRequest {
                generation: 0,
                policy: initial_for_rollback,
                request_id: "rollback-generation-0".to_string(),
                rollback: true,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            control.safe_point(),
            SafePointAction::ApplyPolicy { generation: 0, .. }
        ));
        control.complete_policy(Ok(()), true);
        rollback.join().expect("rollback thread").expect("rollback");
        assert_eq!(control.status().policy, initial);
        assert_eq!(control.status().effective_generation, 0);
    }
}
