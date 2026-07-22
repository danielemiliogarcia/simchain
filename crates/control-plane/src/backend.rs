//! Domain-facing worker and Bitcoin-RPC ports.

use simchain_common::internal_api::{
    CommandAck, DesiredState, LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest,
    MiningWorkerStatus, NetworkAgentStatus, NetworkCommandAck, NetworkLeaseReleaseRequest,
    NetworkLeaseRequest, SpamWorkerStatus,
};
use simchain_common::live_tuning::{MiningTuning, SpamTuning};
use std::time::Duration;

/// Narrow mining-worker control client used by service methods and jobs.
pub trait MiningControlBackend: Send + Sync {
    fn status(&self) -> anyhow::Result<MiningWorkerStatus>;
    fn set_state(&self, state: DesiredState) -> anyhow::Result<CommandAck>;
    fn set_policy(&self, generation: u64, policy: MiningTuning) -> anyhow::Result<CommandAck>;
    fn restore_policy(&self, generation: u64, policy: MiningTuning) -> anyhow::Result<CommandAck>;
    #[allow(dead_code)] // Phase 4 job coordination consumes this worker lease protocol.
    fn acquire_lease(&self, request: LeaseRequest) -> anyhow::Result<CommandAck>;
    #[allow(dead_code)] // Phase 4 job coordination consumes this worker lease protocol.
    fn renew_lease(&self, lease_id: &str, request: LeaseRenewRequest)
        -> anyhow::Result<CommandAck>;
    #[allow(dead_code)] // Phase 4 job coordination consumes this worker lease protocol.
    fn release_lease(
        &self,
        lease_id: &str,
        request: LeaseReleaseRequest,
    ) -> anyhow::Result<CommandAck>;
}

/// Narrow spam-worker control client. It deliberately mirrors the mining
/// control port so mutation jobs can lease both workers uniformly.
pub trait SpamControlBackend: Send + Sync {
    fn status(&self) -> anyhow::Result<SpamWorkerStatus>;
    fn set_state(&self, state: DesiredState) -> anyhow::Result<CommandAck>;
    fn set_policy(&self, generation: u64, policy: SpamTuning) -> anyhow::Result<CommandAck>;
    fn restore_policy(&self, generation: u64, policy: SpamTuning) -> anyhow::Result<CommandAck>;
    #[allow(dead_code)] // Phase 4 job coordination consumes this lease protocol.
    fn acquire_lease(&self, request: LeaseRequest) -> anyhow::Result<CommandAck>;
    #[allow(dead_code)] // Phase 4 job coordination consumes this lease protocol.
    fn renew_lease(&self, lease_id: &str, request: LeaseRenewRequest)
        -> anyhow::Result<CommandAck>;
    #[allow(dead_code)] // Phase 4 job coordination consumes this lease protocol.
    fn release_lease(
        &self,
        lease_id: &str,
        request: LeaseReleaseRequest,
    ) -> anyhow::Result<CommandAck>;
}

/// Private namespace-local network-agent clients. Node names are normalized
/// to `node1`, `node2`, or `node3` by implementations.
pub trait NetworkControlBackend: Send + Sync {
    fn status(&self, node: &str) -> anyhow::Result<NetworkAgentStatus>;
    fn acquire_lease(
        &self,
        node: &str,
        request: NetworkLeaseRequest,
    ) -> anyhow::Result<NetworkCommandAck>;
    fn renew_lease(
        &self,
        node: &str,
        lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<NetworkCommandAck>;
    fn release_lease(
        &self,
        node: &str,
        lease_id: &str,
        request: NetworkLeaseReleaseRequest,
    ) -> anyhow::Result<NetworkCommandAck>;
}

/// Bitcoin-RPC validation used by desired-state transactions. The wait hook
/// keeps stabilization tests deterministic without hiding transport logic.
pub trait ChainBackend: Send + Sync {
    fn node1_height(&self) -> anyhow::Result<u64>;
    fn spam_min_fee(&self) -> anyhow::Result<f64>;
    fn wait(&self, duration: Duration);
}
