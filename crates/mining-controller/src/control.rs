//! Mining-worker control state and safe-point coordination.

use crate::rng::{entropy_seed, Rng};
use simchain_common::internal_api::{
    CommandAck, DesiredState, LastMinedBlock, LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest,
    MiningWorkerStatus, PauseLease, SetMiningPolicyRequest, SetStateRequest, WorkerPhase,
};
use simchain_common::live_tuning::MiningTuning;
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
    policy: MiningTuning,
}

#[derive(Clone)]
struct CompletedRequest {
    fingerprint: String,
    ack: CommandAck,
}

struct Inner {
    desired_state: DesiredState,
    leases: HashMap<String, LeaseEntry>,
    policy: MiningTuning,
    generation: u64,
    pending_policy: Option<PendingPolicy>,
    phase: WorkerPhase,
    effective_rng_seed: u64,
    height: Option<u64>,
    next_scheduled_attempt_ms: Option<u64>,
    last_mined_block: Option<LastMinedBlock>,
    in_flight: bool,
    last_error: Option<String>,
    started: Instant,
    completed_requests: HashMap<String, CompletedRequest>,
    completed_request_order: VecDeque<String>,
}

pub struct MiningControl {
    inner: Mutex<Inner>,
    changed: Condvar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntervalWait {
    Ready,
    Interrupted,
}

impl MiningControl {
    pub fn new(initial_policy: MiningTuning, seed: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                desired_state: DesiredState::Running,
                leases: HashMap::new(),
                policy: initial_policy,
                generation: 0,
                pending_policy: None,
                phase: WorkerPhase::Bootstrapping,
                effective_rng_seed: seed,
                height: None,
                next_scheduled_attempt_ms: None,
                last_mined_block: None,
                in_flight: false,
                last_error: None,
                started: Instant::now(),
                completed_requests: HashMap::new(),
                completed_request_order: VecDeque::new(),
            }),
            changed: Condvar::new(),
        })
    }

    pub fn status(&self) -> MiningWorkerStatus {
        let mut inner = self.inner.lock().expect("mining control lock");
        expire_leases(&mut inner);
        status_from(&inner)
    }

    pub fn set_state(&self, request: SetStateRequest) -> anyhow::Result<CommandAck> {
        validate_request_id(&request.request_id)?;
        let fingerprint = request_fingerprint("state", &request)?;
        let mut inner = self.inner.lock().expect("mining control lock");
        if let Some(ack) = completed_request(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        inner.desired_state = request.state;
        if request.state == DesiredState::Paused && !is_effectively_paused(&inner) {
            inner.phase = WorkerPhase::Pausing;
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
        let mut inner = self.inner.lock().expect("mining control lock");
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
        let expires_at_ms = now_ms().saturating_add(request.ttl_secs.saturating_mul(1000));
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
        if !is_effectively_paused(&inner) {
            inner.phase = WorkerPhase::Pausing;
        }
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
        let mut inner = self.inner.lock().expect("mining control lock");
        expire_leases(&mut inner);
        if let Some(ack) = completed_request(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        let lease = inner
            .leases
            .get_mut(lease_id)
            .ok_or_else(|| anyhow::anyhow!("lease not found"))?;
        lease.deadline = Instant::now() + Duration::from_secs(request.ttl_secs);
        lease.view.expires_at_ms = now_ms().saturating_add(request.ttl_secs.saturating_mul(1000));
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
        let mut inner = self.inner.lock().expect("mining control lock");
        expire_leases(&mut inner);
        if let Some(ack) = completed_request(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        inner.leases.remove(lease_id);
        if request.chain_changed {
            tracing::info!(lease_id, "mining pause lease released after a chain change");
        }
        self.changed.notify_all();
        let ack = ack(&inner, request.request_id.clone());
        remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    pub fn set_policy(&self, request: SetMiningPolicyRequest) -> anyhow::Result<CommandAck> {
        validate_request_id(&request.request_id)?;
        validate_policy(&request.policy)?;
        let fingerprint = request_fingerprint("policy", &request)?;
        let mut inner = self.inner.lock().expect("mining control lock");
        if let Some(ack) = completed_request(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        if request.generation < inner.generation && !request.rollback {
            anyhow::bail!("policy generation is stale");
        }
        if request.generation == inner.generation {
            if request.policy != inner.policy {
                if !request.rollback {
                    anyhow::bail!("generation already identifies a different policy");
                }
            } else {
                let ack = ack(&inner, request.request_id.clone());
                remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
                return Ok(ack);
            }
        }
        if let Some(pending) = &inner.pending_policy {
            if pending.generation == request.generation && pending.policy == request.policy {
                inner = self.wait_for_generation(inner, request.generation, SAFE_POINT_TIMEOUT)?;
                let ack = ack(&inner, request.request_id.clone());
                remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
                return Ok(ack);
            }
            anyhow::bail!("another policy generation is pending");
        }
        inner.pending_policy = Some(PendingPolicy {
            generation: request.generation,
            policy: request.policy,
        });
        self.changed.notify_all();
        inner = self.wait_for_generation(inner, request.generation, SAFE_POINT_TIMEOUT)?;
        let ack = ack(&inner, request.request_id.clone());
        remember_request(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    /// Safe point between bootstrap stages. A requested pause is acknowledged
    /// here; policy changes become effective for the post-bootstrap scheduler.
    pub fn bootstrap_safe_point(&self, height: u64) {
        let mut inner = self.inner.lock().expect("mining control lock");
        inner.height = Some(height);
        apply_pending_policy(&mut inner);
        loop {
            expire_leases(&mut inner);
            if !pause_requested(&inner) {
                inner.phase = WorkerPhase::Bootstrapping;
                self.changed.notify_all();
                return;
            }
            inner.phase = WorkerPhase::Paused;
            inner.next_scheduled_attempt_ms = None;
            self.changed.notify_all();
            inner = self.wait_for_change_or_expiry(inner);
            apply_pending_policy(&mut inner);
        }
    }

    /// Scheduler boundary before choosing a miner or submitting an RPC.
    pub fn mining_safe_point(
        &self,
        rng: &mut Rng,
        toggle: &mut bool,
        observed_generation: &mut u64,
    ) -> MiningTuning {
        let mut inner = self.inner.lock().expect("mining control lock");
        loop {
            expire_leases(&mut inner);
            apply_pending_policy(&mut inner);
            if inner.generation != *observed_generation {
                *rng = Rng::new(inner.effective_rng_seed);
                *toggle = true;
                *observed_generation = inner.generation;
                tracing::info!(
                    generation = inner.generation,
                    seed = inner.effective_rng_seed,
                    "applied mining policy at scheduler boundary"
                );
            }
            if !pause_requested(&inner) {
                inner.phase = WorkerPhase::Running;
                self.changed.notify_all();
                return inner.policy.clone();
            }
            inner.phase = WorkerPhase::Paused;
            inner.next_scheduled_attempt_ms = None;
            self.changed.notify_all();
            inner = self.wait_for_change_or_expiry(inner);
        }
    }

    /// Atomically crosses the scheduler boundary into an in-flight generate.
    /// A pause or policy request that won the race after the caller's last
    /// safe-point check prevents the RPC from starting.
    pub fn begin_generate(&self, generation: u64) -> bool {
        let mut inner = self.inner.lock().expect("mining control lock");
        expire_leases(&mut inner);
        if pause_requested(&inner)
            || inner.pending_policy.is_some()
            || inner.generation != generation
        {
            self.changed.notify_all();
            return false;
        }
        inner.in_flight = true;
        inner.next_scheduled_attempt_ms = None;
        true
    }

    pub fn finish_generate(&self, mined: Option<LastMinedBlock>, height: Option<u64>) {
        let mut inner = self.inner.lock().expect("mining control lock");
        inner.in_flight = false;
        if let Some(mined) = mined {
            inner.last_mined_block = Some(mined);
            inner.last_error = None;
        }
        if height.is_some() {
            inner.height = height;
        }
        if pause_requested(&inner) {
            inner.phase = WorkerPhase::Paused;
        } else {
            inner.phase = WorkerPhase::Running;
        }
        self.changed.notify_all();
    }

    pub fn record_error(&self, message: String) {
        let mut inner = self.inner.lock().expect("mining control lock");
        inner.last_error = Some(message);
    }

    pub fn wait_interval(&self, duration: Duration, generation: u64) -> IntervalWait {
        let deadline = Instant::now() + duration;
        let mut inner = self.inner.lock().expect("mining control lock");
        inner.next_scheduled_attempt_ms =
            Some(now_ms().saturating_add(duration.as_millis() as u64));
        loop {
            expire_leases(&mut inner);
            if pause_requested(&inner)
                || inner.pending_policy.is_some()
                || inner.generation != generation
            {
                inner.next_scheduled_attempt_ms = None;
                self.changed.notify_all();
                return IntervalWait::Interrupted;
            }
            let now = Instant::now();
            if now >= deadline {
                inner.next_scheduled_attempt_ms = None;
                return IntervalWait::Ready;
            }
            let wait = (deadline - now).min(time_until_lease_expiry(&inner));
            let (next, _) = self
                .changed
                .wait_timeout(inner, wait)
                .expect("mining control wait");
            inner = next;
        }
    }

    pub fn current_generation(&self) -> u64 {
        self.inner.lock().expect("mining control lock").generation
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
                anyhow::bail!("timed out waiting for mining safe point");
            }
            let (next, result) = self
                .changed
                .wait_timeout(inner, deadline - now)
                .expect("mining control wait");
            inner = next;
            if result.timed_out() && !is_effectively_paused(&inner) {
                anyhow::bail!("timed out waiting for mining safe point");
            }
        }
        Ok(inner)
    }

    fn wait_for_generation<'a>(
        &self,
        mut inner: MutexGuard<'a, Inner>,
        generation: u64,
        timeout: Duration,
    ) -> anyhow::Result<MutexGuard<'a, Inner>> {
        let deadline = Instant::now() + timeout;
        while inner.generation != generation {
            let now = Instant::now();
            if now >= deadline {
                anyhow::bail!("timed out waiting for mining policy safe point");
            }
            let (next, result) = self
                .changed
                .wait_timeout(inner, deadline - now)
                .expect("mining control wait");
            inner = next;
            if result.timed_out() && inner.generation != generation {
                anyhow::bail!("timed out waiting for mining policy safe point");
            }
        }
        Ok(inner)
    }

    fn wait_for_change_or_expiry<'a>(&self, inner: MutexGuard<'a, Inner>) -> MutexGuard<'a, Inner> {
        let wait = time_until_lease_expiry(&inner);
        self.changed
            .wait_timeout(inner, wait)
            .expect("mining control wait")
            .0
    }

    fn rollback_unacquired_lease(
        &self,
        lease_id: &str,
        tentative: &LeaseEntry,
        previous: Option<LeaseEntry>,
    ) {
        let mut inner = self.inner.lock().expect("mining control lock");
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

fn validate_policy(policy: &MiningTuning) -> anyhow::Result<()> {
    let source: BTreeMap<String, String> = policy
        .canonical_values()
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    let reparsed = MiningTuning::from_source(&source)?;
    if &reparsed != policy {
        anyhow::bail!("mining policy is not canonical");
    }
    Ok(())
}

fn pause_requested(inner: &Inner) -> bool {
    inner.desired_state == DesiredState::Paused || !inner.leases.is_empty()
}

fn is_effectively_paused(inner: &Inner) -> bool {
    inner.phase == WorkerPhase::Paused && !inner.in_flight
}

fn apply_pending_policy(inner: &mut Inner) {
    let Some(pending) = inner.pending_policy.take() else {
        return;
    };
    inner.policy = pending.policy;
    inner.generation = pending.generation;
    inner.effective_rng_seed = inner.policy.rng_seed.unwrap_or_else(entropy_seed);
}

fn expire_leases(inner: &mut Inner) {
    let now = Instant::now();
    inner.leases.retain(|_, lease| lease.deadline > now);
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

fn status_from(inner: &Inner) -> MiningWorkerStatus {
    let mut active_leases: Vec<PauseLease> = inner
        .leases
        .values()
        .map(|lease| lease.view.clone())
        .collect();
    active_leases.sort_by(|left, right| left.lease_id.cmp(&right.lease_id));
    MiningWorkerStatus {
        component: "mining".to_string(),
        phase: inner.phase,
        desired_state: inner.desired_state,
        effective_state: if is_effectively_paused(inner) {
            DesiredState::Paused
        } else {
            DesiredState::Running
        },
        policy: inner.policy.clone(),
        effective_generation: inner.generation,
        effective_rng_seed: inner.effective_rng_seed,
        height: inner.height,
        next_scheduled_attempt_ms: inner.next_scheduled_attempt_ms,
        last_mined_block: inner.last_mined_block.clone(),
        active_leases,
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

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use simchain_common::live_tuning;

    fn policy() -> MiningTuning {
        MiningTuning::from_source(&live_tuning::staged_map(&BTreeMap::new()))
            .expect("default mining policy")
    }

    #[test]
    fn manual_pause_is_acknowledged_only_at_a_safe_point() {
        let control = MiningControl::new(policy(), 7);
        let worker = control.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            worker.bootstrap_safe_point(10);
        });
        let ack = control
            .set_state(SetStateRequest {
                state: DesiredState::Paused,
                request_id: "pause-1".to_string(),
            })
            .expect("pause");
        assert_eq!(ack.phase, WorkerPhase::Paused);
        assert_eq!(control.status().effective_state, DesiredState::Paused);
        control
            .set_state(SetStateRequest {
                state: DesiredState::Running,
                request_id: "resume-1".to_string(),
            })
            .expect("resume");
        handle.join().expect("worker");
    }

    #[test]
    fn lease_is_idempotent_and_does_not_override_manual_pause() {
        let control = MiningControl::new(policy(), 7);
        let worker = control.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            worker.bootstrap_safe_point(10);
        });
        control
            .set_state(SetStateRequest {
                state: DesiredState::Paused,
                request_id: "manual".to_string(),
            })
            .expect("manual pause");

        // Acquire the same lease twice while manual pause already owns the
        // effective pause state.
        let request = LeaseRequest {
            lease_id: "lease-1".to_string(),
            owner_job_id: "job-1".to_string(),
            purpose: "test".to_string(),
            ttl_secs: 1,
            request_id: "lease-request".to_string(),
        };
        control.acquire_lease(request.clone()).expect("lease");
        control.acquire_lease(request).expect("idempotent lease");
        assert_eq!(control.status().active_leases.len(), 1);
        control
            .set_state(SetStateRequest {
                state: DesiredState::Running,
                request_id: "manual-resume".to_string(),
            })
            .expect("manual resume");
        control
            .release_lease(
                "lease-1",
                LeaseReleaseRequest {
                    chain_changed: false,
                    request_id: "release".to_string(),
                },
            )
            .expect("release");
        handle.join().expect("worker");
    }

    #[test]
    fn policy_change_applies_at_scheduler_boundary_and_resets_seed() {
        let control = MiningControl::new(policy(), 7);
        control.bootstrap_safe_point(204);
        let mut changed = policy();
        changed.mean_secs = 12;
        changed.rng_seed = Some(42);
        let setter = control.clone();
        let policy_for_setter = changed.clone();
        let handle = std::thread::spawn(move || {
            setter.set_policy(SetMiningPolicyRequest {
                generation: 1,
                policy: policy_for_setter,
                request_id: "policy-1".to_string(),
                rollback: false,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        let mut rng = Rng::new(7);
        let mut toggle = false;
        let mut generation = 0;
        let effective = control.mining_safe_point(&mut rng, &mut toggle, &mut generation);
        let ack = handle.join().expect("setter").expect("policy");
        assert_eq!(ack.effective_generation, 1);
        assert_eq!(effective, changed);
        assert_eq!(control.status().effective_rng_seed, 42);
    }

    #[test]
    fn interval_wait_wakes_for_pause() {
        let control = MiningControl::new(policy(), 7);
        control.bootstrap_safe_point(204);
        let worker = control.clone();
        let handle = std::thread::spawn(move || {
            worker.wait_interval(Duration::from_secs(30), worker.current_generation())
        });
        std::thread::sleep(Duration::from_millis(20));
        {
            let mut inner = control.inner.lock().expect("lock");
            inner.desired_state = DesiredState::Paused;
            control.changed.notify_all();
        }
        assert_eq!(handle.join().expect("waiter"), IntervalWait::Interrupted);
    }

    #[test]
    fn transaction_rollback_can_restore_an_older_generation() {
        let initial = policy();
        let control = MiningControl::new(initial.clone(), 7);
        control.bootstrap_safe_point(204);
        let mut rng = Rng::new(7);
        let mut toggle = true;
        let mut observed_generation = 0;

        let mut changed = initial.clone();
        changed.mean_secs = 12;
        let setter = control.clone();
        let changed_for_setter = changed.clone();
        let apply = std::thread::spawn(move || {
            setter.set_policy(SetMiningPolicyRequest {
                generation: 3,
                policy: changed_for_setter,
                request_id: "apply-generation-3".to_string(),
                rollback: false,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        control.mining_safe_point(&mut rng, &mut toggle, &mut observed_generation);
        apply.join().expect("apply thread").expect("apply policy");

        let rollback_control = control.clone();
        let initial_for_rollback = initial.clone();
        let rollback = std::thread::spawn(move || {
            rollback_control.set_policy(SetMiningPolicyRequest {
                generation: 0,
                policy: initial_for_rollback,
                request_id: "rollback-generation-0".to_string(),
                rollback: true,
            })
        });
        std::thread::sleep(Duration::from_millis(20));
        let restored = control.mining_safe_point(&mut rng, &mut toggle, &mut observed_generation);
        rollback
            .join()
            .expect("rollback thread")
            .expect("rollback policy");
        assert_eq!(restored, initial);
        assert_eq!(control.status().effective_generation, 0);
    }

    #[test]
    fn completed_request_ids_are_idempotent_and_cannot_be_reused() {
        let control = MiningControl::new(policy(), 7);
        let request = SetStateRequest {
            state: DesiredState::Running,
            request_id: "same-request".to_string(),
        };
        let first = control.set_state(request.clone()).expect("first request");
        let second = control.set_state(request).expect("idempotent retry");
        assert_eq!(first, second);

        let error = control
            .set_state(SetStateRequest {
                state: DesiredState::Paused,
                request_id: "same-request".to_string(),
            })
            .expect_err("request ID reuse must fail");
        assert!(error.to_string().contains("different command"));
    }

    #[test]
    fn an_expired_lease_releases_the_safe_point() {
        let control = MiningControl::new(policy(), 7);
        let worker = control.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            worker.bootstrap_safe_point(204);
        });
        control
            .acquire_lease(LeaseRequest {
                lease_id: "expiring".to_string(),
                owner_job_id: "job".to_string(),
                purpose: "test crash recovery".to_string(),
                ttl_secs: 60,
                request_id: "acquire-expiring".to_string(),
            })
            .expect("lease");
        {
            let mut inner = control.inner.lock().expect("control lock");
            inner.leases.get_mut("expiring").expect("lease").deadline = Instant::now();
            control.changed.notify_all();
        }
        handle.join().expect("worker resumed");
        assert!(control.status().active_leases.is_empty());
        assert_eq!(control.status().effective_state, DesiredState::Running);
    }

    #[test]
    fn failed_lease_acquire_does_not_leave_orphan_pause() {
        let control = MiningControl::new(policy(), 7);
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
            .contains("timed out waiting for mining safe point"));
        assert!(control.status().active_leases.is_empty());
        assert_eq!(control.status().effective_state, DesiredState::Running);
    }
}
