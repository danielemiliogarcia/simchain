//! Single active impairment lease with idempotent commands and TTL healing.

use crate::system::NetworkSystem;
use serde::Serialize;
use simchain_common::internal_api::{
    LeaseRenewRequest, NetworkAgentStatus, NetworkCommandAck, NetworkImpairment,
    NetworkImpairmentLease, NetworkLeaseReleaseRequest, NetworkLeaseRequest,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_REQUEST_HISTORY: usize = 256;
const MAX_LEASE_TTL_SECS: u64 = 3600;

struct ActiveLease {
    view: NetworkImpairmentLease,
    deadline: Instant,
}

#[derive(Clone)]
struct RememberedRequest {
    fingerprint: String,
    ack: NetworkCommandAck,
}

struct Inner {
    generation: u64,
    active: Option<ActiveLease>,
    last_error: Option<String>,
    requests: HashMap<String, RememberedRequest>,
    request_order: VecDeque<String>,
}

pub struct NetworkControl {
    node: String,
    interface: String,
    started: Instant,
    system: Arc<dyn NetworkSystem>,
    inner: Mutex<Inner>,
}

impl NetworkControl {
    pub fn new(node: String, system: Arc<dyn NetworkSystem>) -> anyhow::Result<Self> {
        let interface = system.detect_p2p_interface()?;
        // A restarted agent shares the node's still-live namespace. Clear any
        // state left by a previous process before accepting requests.
        system.clear(&interface)?;
        Ok(Self {
            node,
            interface,
            started: Instant::now(),
            system,
            inner: Mutex::new(Inner {
                generation: 0,
                active: None,
                last_error: None,
                requests: HashMap::new(),
                request_order: VecDeque::new(),
            }),
        })
    }

    pub fn spawn_expiry_worker(self: &Arc<Self>) -> anyhow::Result<std::thread::JoinHandle<()>> {
        let control = self.clone();
        std::thread::Builder::new()
            .name(format!("network-lease-expiry-{}", self.node))
            .spawn(move || loop {
                control.expire_if_needed();
                std::thread::sleep(Duration::from_millis(250));
            })
            .map_err(Into::into)
    }

    pub fn status(&self) -> NetworkAgentStatus {
        self.expire_if_needed();
        let inner = self.inner.lock().expect("network control lock");
        NetworkAgentStatus {
            component: "network-agent".to_string(),
            node: self.node.clone(),
            p2p_interface: self.interface.clone(),
            effective_generation: inner.generation,
            active_lease: inner.active.as_ref().map(|active| active.view.clone()),
            uptime_secs: self.started.elapsed().as_secs(),
            last_error: inner.last_error.clone(),
        }
    }

    pub fn acquire(&self, request: NetworkLeaseRequest) -> anyhow::Result<NetworkCommandAck> {
        validate_acquire(&request)?;
        let fingerprint = fingerprint("acquire", &request)?;
        let mut inner = self.inner.lock().expect("network control lock");
        self.expire_locked(&mut inner);
        if let Some(ack) = replay(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        if let Some(active) = inner.active.as_ref() {
            anyhow::ensure!(
                active.view.lease_id == request.lease_id
                    && active.view.owner_job_id == request.owner_job_id
                    && active.view.purpose == request.purpose
                    && active.view.impairment == request.impairment,
                "another network impairment lease is active"
            );
            let ack = ack(&request.request_id, inner.generation, true);
            remember(&mut inner, request.request_id, fingerprint, ack.clone());
            return Ok(ack);
        }

        if let Err(error) = self.system.apply(&self.interface, &request.impairment) {
            inner.last_error = Some(format!("failed to apply impairment: {error}"));
            return Err(error);
        }
        inner.generation = inner.generation.saturating_add(1);
        inner.last_error = None;
        inner.active = Some(ActiveLease {
            view: NetworkImpairmentLease {
                lease_id: request.lease_id,
                owner_job_id: request.owner_job_id,
                purpose: request.purpose,
                expires_at_ms: now_ms().saturating_add(request.ttl_secs.saturating_mul(1000)),
                impairment: request.impairment,
            },
            deadline: Instant::now() + Duration::from_secs(request.ttl_secs),
        });
        let ack = ack(&request.request_id, inner.generation, true);
        remember(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    pub fn renew(
        &self,
        lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<NetworkCommandAck> {
        anyhow::ensure!(!lease_id.trim().is_empty(), "lease ID must not be empty");
        anyhow::ensure!(request.ttl_secs > 0, "lease ttl_secs must be positive");
        anyhow::ensure!(
            request.ttl_secs <= MAX_LEASE_TTL_SECS,
            "lease ttl_secs must not exceed {MAX_LEASE_TTL_SECS}"
        );
        anyhow::ensure!(
            !request.request_id.trim().is_empty(),
            "request ID must not be empty"
        );
        let fingerprint = fingerprint(&format!("renew:{lease_id}"), &request)?;
        let mut inner = self.inner.lock().expect("network control lock");
        self.expire_locked(&mut inner);
        if let Some(ack) = replay(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        let generation = inner.generation;
        let active = inner
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("network impairment lease not found"))?;
        anyhow::ensure!(
            active.view.lease_id == lease_id,
            "network impairment lease not found"
        );
        active.deadline = Instant::now() + Duration::from_secs(request.ttl_secs);
        active.view.expires_at_ms = now_ms().saturating_add(request.ttl_secs.saturating_mul(1000));
        let ack = ack(&request.request_id, generation, true);
        remember(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    pub fn release(
        &self,
        lease_id: &str,
        request: NetworkLeaseReleaseRequest,
    ) -> anyhow::Result<NetworkCommandAck> {
        anyhow::ensure!(!lease_id.trim().is_empty(), "lease ID must not be empty");
        anyhow::ensure!(
            !request.request_id.trim().is_empty(),
            "request ID must not be empty"
        );
        let fingerprint = fingerprint(&format!("release:{lease_id}"), &request)?;
        let mut inner = self.inner.lock().expect("network control lock");
        self.expire_locked(&mut inner);
        if let Some(ack) = replay(&inner, &request.request_id, &fingerprint)? {
            return Ok(ack);
        }
        match inner.active.as_ref() {
            Some(active) => anyhow::ensure!(
                active.view.lease_id == lease_id,
                "a different network impairment lease is active"
            ),
            None => {
                let ack = ack(&request.request_id, inner.generation, false);
                remember(&mut inner, request.request_id, fingerprint, ack.clone());
                return Ok(ack);
            }
        }
        if let Err(error) = self.system.clear(&self.interface) {
            inner.last_error = Some(format!("failed to clear impairment: {error}"));
            return Err(error);
        }
        inner.active = None;
        inner.generation = inner.generation.saturating_add(1);
        inner.last_error = None;
        let ack = ack(&request.request_id, inner.generation, false);
        remember(&mut inner, request.request_id, fingerprint, ack.clone());
        Ok(ack)
    }

    fn expire_if_needed(&self) {
        let mut inner = self.inner.lock().expect("network control lock");
        self.expire_locked(&mut inner);
    }

    fn expire_locked(&self, inner: &mut Inner) {
        let expired = inner
            .active
            .as_ref()
            .is_some_and(|active| active.deadline <= Instant::now());
        if !expired {
            return;
        }
        match self.system.clear(&self.interface) {
            Ok(()) => {
                if let Some(active) = inner.active.take() {
                    tracing::warn!(
                        node = %self.node,
                        lease_id = %active.view.lease_id,
                        "expired network lease healed automatically"
                    );
                }
                inner.generation = inner.generation.saturating_add(1);
                inner.last_error = None;
            }
            Err(error) => {
                let message = format!("failed to heal expired impairment: {error}");
                if inner.last_error.as_deref() != Some(&message) {
                    tracing::error!(node = %self.node, "{message}");
                }
                inner.last_error = Some(message);
            }
        }
    }
}

fn validate_acquire(request: &NetworkLeaseRequest) -> anyhow::Result<()> {
    anyhow::ensure!(
        !request.lease_id.trim().is_empty()
            && !request.owner_job_id.trim().is_empty()
            && !request.purpose.trim().is_empty()
            && !request.request_id.trim().is_empty(),
        "lease identifiers, owner, purpose, and request ID must be non-empty"
    );
    anyhow::ensure!(request.ttl_secs > 0, "lease ttl_secs must be positive");
    anyhow::ensure!(
        request.ttl_secs <= MAX_LEASE_TTL_SECS,
        "lease ttl_secs must not exceed {MAX_LEASE_TTL_SECS}"
    );
    match request.impairment {
        NetworkImpairment::Netem { delay_ms, loss_pct } => {
            anyhow::ensure!(delay_ms <= 600_000, "delay_ms must not exceed 600000");
            anyhow::ensure!(loss_pct.is_finite(), "loss_pct must be finite");
            anyhow::ensure!(
                (0.0..=100.0).contains(&loss_pct),
                "loss_pct must be from 0 through 100"
            );
            anyhow::ensure!(
                delay_ms > 0 || loss_pct > 0.0,
                "netem must specify delay or loss"
            );
        }
        NetworkImpairment::Partition {
            ingress_drop,
            egress_drop,
        } => anyhow::ensure!(
            ingress_drop || egress_drop,
            "partition must drop ingress or egress"
        ),
    }
    Ok(())
}

fn fingerprint(operation: &str, value: &impl Serialize) -> anyhow::Result<String> {
    Ok(format!("{operation}:{}", serde_json::to_string(value)?))
}

fn replay(
    inner: &Inner,
    request_id: &str,
    fingerprint: &str,
) -> anyhow::Result<Option<NetworkCommandAck>> {
    let Some(remembered) = inner.requests.get(request_id) else {
        return Ok(None);
    };
    anyhow::ensure!(
        remembered.fingerprint == fingerprint,
        "request ID was already used with different content"
    );
    Ok(Some(remembered.ack.clone()))
}

fn remember(inner: &mut Inner, request_id: String, fingerprint: String, ack: NetworkCommandAck) {
    if !inner.requests.contains_key(&request_id) {
        inner.request_order.push_back(request_id.clone());
    }
    inner
        .requests
        .insert(request_id, RememberedRequest { fingerprint, ack });
    while inner.request_order.len() > MAX_REQUEST_HISTORY {
        if let Some(oldest) = inner.request_order.pop_front() {
            inner.requests.remove(&oldest);
        }
    }
}

fn ack(request_id: &str, generation: u64, active: bool) -> NetworkCommandAck {
    NetworkCommandAck {
        request_id: request_id.to_string(),
        effective_generation: generation,
        impairment_active: active,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockSystem {
        applies: AtomicUsize,
        clears: AtomicUsize,
    }

    impl MockSystem {
        fn new() -> Self {
            Self {
                applies: AtomicUsize::new(0),
                clears: AtomicUsize::new(0),
            }
        }
    }

    impl NetworkSystem for MockSystem {
        fn detect_p2p_interface(&self) -> anyhow::Result<String> {
            Ok("eth1".to_string())
        }

        fn apply(&self, _interface: &str, _impairment: &NetworkImpairment) -> anyhow::Result<()> {
            self.applies.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn clear(&self, _interface: &str) -> anyhow::Result<()> {
            self.clears.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn request(id: &str) -> NetworkLeaseRequest {
        NetworkLeaseRequest {
            lease_id: id.to_string(),
            owner_job_id: "job-1".to_string(),
            purpose: "test".to_string(),
            ttl_secs: 30,
            request_id: format!("{id}-acquire"),
            impairment: NetworkImpairment::Partition {
                ingress_drop: true,
                egress_drop: true,
            },
        }
    }

    #[test]
    fn acquisition_is_idempotent_and_conflicts_with_another_lease() {
        let system = Arc::new(MockSystem::new());
        let control = NetworkControl::new("node3".to_string(), system.clone()).expect("control");
        let first = control.acquire(request("lease-1")).expect("acquire");
        let repeated = control.acquire(request("lease-1")).expect("replay");
        assert_eq!(first, repeated);
        assert_eq!(system.applies.load(Ordering::Relaxed), 1);
        assert!(control.acquire(request("lease-2")).is_err());
    }

    #[test]
    fn stale_release_cannot_clear_a_newer_active_lease() {
        let system = Arc::new(MockSystem::new());
        let control = NetworkControl::new("node3".to_string(), system.clone()).expect("control");
        control.acquire(request("lease-new")).expect("acquire");
        let before = system.clears.load(Ordering::Relaxed);
        assert!(control
            .release(
                "lease-old",
                NetworkLeaseReleaseRequest {
                    request_id: "release-old".to_string()
                }
            )
            .is_err());
        assert_eq!(system.clears.load(Ordering::Relaxed), before);
        assert_eq!(
            control.status().active_lease.expect("active").lease_id,
            "lease-new"
        );
    }

    #[test]
    fn expiry_heals_and_removes_the_lease() {
        let system = Arc::new(MockSystem::new());
        let control = NetworkControl::new("node3".to_string(), system.clone()).expect("control");
        control.acquire(request("lease-1")).expect("acquire");
        {
            let mut inner = control.inner.lock().expect("lock");
            inner.active.as_mut().expect("active").deadline = Instant::now();
        }
        let status = control.status();
        assert!(status.active_lease.is_none());
        // One startup clear plus one TTL heal.
        assert_eq!(system.clears.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn validation_rejects_non_finite_loss_and_noop_netem() {
        let mut value = request("lease");
        value.impairment = NetworkImpairment::Netem {
            delay_ms: 0,
            loss_pct: f64::NAN,
        };
        assert!(validate_acquire(&value).is_err());
        value.impairment = NetworkImpairment::Netem {
            delay_ms: 0,
            loss_pct: 0.0,
        };
        assert!(validate_acquire(&value).is_err());
        value.ttl_secs = MAX_LEASE_TTL_SECS + 1;
        value.impairment = NetworkImpairment::Partition {
            ingress_drop: true,
            egress_drop: true,
        };
        assert!(validate_acquire(&value).is_err());
    }
}
