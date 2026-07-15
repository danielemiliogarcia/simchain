//! Domain-facing backend ports. The control-plane service and tests depend
//! on these traits, not on Docker/Compose. Phase 1 keeps a legacy Compose
//! adapter behind them; later phases replace it component by component.

use simchain_common::internal_api::{
    CommandAck, DesiredState, LastMinedBlock, LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest,
    MiningWorkerStatus, SpamWorkerStatus,
};
use simchain_common::live_tuning::{MiningTuning, SpamTuning};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ComponentInfo {
    pub present: bool,
    pub status: String,
    pub running: bool,
    pub restarting: bool,
    pub exit_code: i64,
    pub restart_count: i64,
    pub effective_config: HashMap<String, String>,
    pub phase: Option<String>,
    pub effective_generation: Option<u64>,
    pub uptime_secs: Option<u64>,
    pub last_error: Option<String>,
    pub desired_state: Option<DesiredState>,
    pub effective_state: Option<DesiredState>,
    pub observed_height: Option<u64>,
    pub next_scheduled_attempt_ms: Option<u64>,
    pub last_mined_block: Option<LastMinedBlock>,
    pub active_lease_count: Option<usize>,
    pub cycle_phase: Option<String>,
    pub accepted_transactions: Option<u64>,
    pub reconciliation_pending: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct BackendOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl BackendOutput {
    pub fn tail(&self, lines: usize) -> String {
        let combined = format!("{}\n{}", self.stdout.trim(), self.stderr.trim());
        let all: Vec<&str> = combined
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let start = all.len().saturating_sub(lines);
        all[start..].join("\n")
    }
}

/// Component reachability, lifecycle telemetry, and effective configuration.
pub trait ComponentBackend: Send + Sync {
    fn inspect_components(&self, names: &[&str]) -> anyhow::Result<HashMap<String, ComponentInfo>>;
}

/// Applies desired runtime configuration. Method names are deliberately
/// transport-neutral even though the Phase-1 adapter recreates containers.
pub trait ConfigurationBackend: Send + Sync {
    fn apply_configuration(
        &self,
        components: &[String],
        desired: &BTreeMap<String, String>,
        generation: u64,
    ) -> anyhow::Result<BackendOutput>;

    fn restore_configuration(
        &self,
        components: &[String],
        managed_values: &BTreeMap<String, String>,
        generations: &BTreeMap<String, u64>,
    ) -> anyhow::Result<BackendOutput>;

    fn remove_components(&self, names: &[String]) -> anyhow::Result<BackendOutput>;
}

/// Narrow mining-worker control client used by service methods, the hybrid
/// migration adapter, and later job leases.
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

/// Bounded action/probe port used by configuration and, in later phases, by
/// jobs. It contains no process-lifecycle or Docker vocabulary.
pub trait JobActions: Send + Sync {
    fn node1_height(&self) -> anyhow::Result<u64>;
    fn spam_min_fee(&self) -> anyhow::Result<f64>;
    fn wait(&self, duration: Duration);
}
