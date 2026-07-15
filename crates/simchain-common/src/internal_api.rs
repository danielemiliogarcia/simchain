//! Authenticated HTTP/JSON protocol shared by the control plane and its
//! long-running workers. These endpoints are private to the Compose control
//! network and are never published to the host.

use crate::live_tuning::{MiningTuning, SpamTuning};
use serde::{Deserialize, Serialize};

pub const INTERNAL_API_PREFIX: &str = "/internal/v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Paused,
}

impl DesiredState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    Bootstrapping,
    Initializing,
    Running,
    Active,
    Pausing,
    Paused,
    Reconciling,
    Disabled,
    Error,
}

impl WorkerPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrapping => "bootstrapping",
            Self::Initializing => "initializing",
            Self::Running => "running",
            Self::Active => "active",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Reconciling => "reconciling",
            Self::Disabled => "disabled",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PauseLease {
    pub lease_id: String,
    pub owner_job_id: String,
    pub purpose: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LeaseRequest {
    pub lease_id: String,
    pub owner_job_id: String,
    pub purpose: String,
    pub ttl_secs: u64,
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LeaseRenewRequest {
    pub ttl_secs: u64,
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LeaseReleaseRequest {
    pub chain_changed: bool,
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SetStateRequest {
    pub state: DesiredState,
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SetMiningPolicyRequest {
    pub generation: u64,
    pub policy: MiningTuning,
    pub request_id: String,
    /// Allows the control plane to restore the exact pre-transaction
    /// generation after a later component fails. Normal reconciliation and
    /// user applies must leave this false so stale generations are rejected.
    #[serde(default)]
    pub rollback: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SetSpamPolicyRequest {
    pub generation: u64,
    pub policy: SpamTuning,
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandAck {
    pub request_id: String,
    pub phase: WorkerPhase,
    pub effective_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LastMinedBlock {
    pub height: u64,
    pub hash: String,
    pub miner: String,
    pub mined_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MiningWorkerStatus {
    pub component: String,
    pub phase: WorkerPhase,
    pub desired_state: DesiredState,
    pub effective_state: DesiredState,
    pub policy: MiningTuning,
    pub effective_generation: u64,
    pub effective_rng_seed: u64,
    pub height: Option<u64>,
    pub next_scheduled_attempt_ms: Option<u64>,
    pub last_mined_block: Option<LastMinedBlock>,
    pub active_leases: Vec<PauseLease>,
    pub uptime_secs: u64,
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_state_json_is_stable() {
        assert_eq!(
            serde_json::to_string(&DesiredState::Paused).expect("serialize"),
            "\"paused\""
        );
        assert_eq!(
            serde_json::from_str::<DesiredState>("\"running\"").expect("deserialize"),
            DesiredState::Running
        );
    }
}
