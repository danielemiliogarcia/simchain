use crate::internal_api::{DesiredState, LastMinedBlock, WorkerPhase};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MempoolSummary {
    pub tx_count: usize,
    pub vbytes: usize,
    pub usage_bytes: usize,
    pub min_fee: f64,
    pub min_relay_fee: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BlockSummary {
    pub height: u64,
    pub hash: String,
    pub time: u64,
    pub delta_secs: Option<i64>,
    pub tx_count: usize,
    pub size_bytes: usize,
    pub weight: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Cadence {
    pub mean_secs: f64,
    pub samples: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FeeBucket {
    pub label: String,
    pub count: usize,
}

/// Component state is intentionally domain-oriented. The transitional
/// Docker adapter populates the legacy process fields; worker APIs can add
/// phase, generation, and uptime without changing the outer status shape.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ComponentState {
    pub present: bool,
    pub status: String,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<DesiredState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_state: Option<DesiredState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_height: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_scheduled_attempt_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mined_block: Option<LastMinedBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_lease_count: Option<usize>,
    #[serde(default)]
    pub restarting: bool,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default)]
    pub restart_count: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OperationSummary {
    pub job_id: String,
    pub kind: String,
    pub state: String,
    pub phase: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ImpairmentSummary {
    pub node: String,
    pub kind: String,
    pub owner_job_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExplorerStatus {
    pub url: String,
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct StatusResponse {
    pub height: Option<u64>,
    pub best_hash: Option<String>,
    pub mempool: Option<MempoolSummary>,
    pub recent_blocks: Vec<BlockSummary>,
    pub cadence: Option<Cadence>,
    pub fee_histogram: Vec<FeeBucket>,
    pub components: BTreeMap<String, ComponentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_operation: Option<OperationSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impairments: Vec<ImpairmentSummary>,
    pub desired_generation: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effective_generations: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explorer: Option<ExplorerStatus>,
    pub last_updated_ms: Option<u64>,
    pub slow_last_updated_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub ready: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SetComponentStateRequest {
    pub state: DesiredState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComponentControlResponse {
    pub component: String,
    pub desired_state: DesiredState,
    pub effective_state: DesiredState,
    pub phase: WorkerPhase,
    pub effective_generation: u64,
}
