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
    #[serde(default)]
    pub rollback: bool,
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SpamWorkerStatus {
    pub component: String,
    pub phase: WorkerPhase,
    pub desired_state: DesiredState,
    pub effective_state: DesiredState,
    pub policy: SpamTuning,
    pub effective_generation: u64,
    pub observed_height: Option<u64>,
    pub cycle_phase: Option<String>,
    pub accepted_transactions: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cycle_duration_ms: Option<u64>,
    pub active_leases: Vec<PauseLease>,
    pub reconciliation_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<SpamCapacityStatus>,
    #[serde(default)]
    pub reconciliation_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciliation_reason: Option<String>,
    pub uptime_secs: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpamCapacityState {
    Ready,
    Provisioning,
    CapacityDegraded,
}

impl SpamCapacityState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Provisioning => "provisioning",
            Self::CapacityDegraded => "capacity_degraded",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpamCapacityStatus {
    pub state: SpamCapacityState,
    pub usable_branches_per_miner: u64,
    pub required_branches_per_miner: u64,
    pub target_branches_per_miner: u64,
    #[serde(default)]
    pub branch_provisioning: bool,
    #[serde(default)]
    pub floor_pool_provisioning: bool,
}

/// Network impairment applied only to a node's P2P interface.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NetworkImpairment {
    Netem {
        delay_ms: u64,
        loss_pct: f64,
    },
    Partition {
        ingress_drop: bool,
        egress_drop: bool,
    },
}

impl NetworkImpairment {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Netem { .. } => "netem",
            Self::Partition { .. } => "partition",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NetworkLeaseRequest {
    pub lease_id: String,
    pub owner_job_id: String,
    pub purpose: String,
    pub ttl_secs: u64,
    pub request_id: String,
    pub impairment: NetworkImpairment,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkLeaseReleaseRequest {
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NetworkImpairmentLease {
    pub lease_id: String,
    pub owner_job_id: String,
    pub purpose: String,
    pub expires_at_ms: u64,
    pub impairment: NetworkImpairment,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkCommandAck {
    pub request_id: String,
    pub effective_generation: u64,
    pub impairment_active: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NetworkAgentStatus {
    pub component: String,
    pub node: String,
    pub p2p_interface: String,
    pub effective_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_lease: Option<NetworkImpairmentLease>,
    pub uptime_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
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

    #[test]
    fn network_impairment_json_is_tagged_and_stable() {
        let value = serde_json::to_value(NetworkImpairment::Partition {
            ingress_drop: true,
            egress_drop: true,
        })
        .expect("serialize");
        assert_eq!(value["kind"], "partition");
        assert_eq!(value["ingress_drop"], true);
    }

    #[test]
    fn spam_capacity_exposes_branch_provisioning_and_defaults_old_payloads() {
        let capacity = SpamCapacityStatus {
            state: SpamCapacityState::CapacityDegraded,
            usable_branches_per_miner: 26,
            required_branches_per_miner: 30,
            target_branches_per_miner: 45,
            branch_provisioning: true,
            floor_pool_provisioning: false,
        };
        let value = serde_json::to_value(&capacity).expect("serialize capacity");
        assert_eq!(value["branch_provisioning"], true);

        let mut old_value = value;
        old_value
            .as_object_mut()
            .expect("capacity object")
            .remove("branch_provisioning");
        let decoded: SpamCapacityStatus =
            serde_json::from_value(old_value).expect("deserialize old capacity");
        assert!(!decoded.branch_provisioning);
    }
}
