use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Reorg,
    Scenario,
    Partition,
    Degrade,
    Mine,
    SpamBurst,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reorg => "reorg",
            Self::Scenario => "scenario",
            Self::Partition => "partition",
            Self::Degrade => "degrade",
            Self::Mine => "mine",
            Self::SpamBurst => "spam_burst",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Starting,
    Running,
    WaitingAtCheckpoint,
    AbortRequested,
    Succeeded,
    Failed,
    Aborted,
    Interrupted,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::WaitingAtCheckpoint => "waiting_at_checkpoint",
            Self::AbortRequested => "abort_requested",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Aborted | Self::Interrupted
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    NotStarted,
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReorgJobRequest {
    #[serde(default = "default_reorg_depth")]
    pub depth: u64,
    #[serde(default)]
    pub empty: bool,
    #[serde(default = "default_reorg_node")]
    pub node: String,
    #[serde(default)]
    pub adds_new_txs: u64,
    #[serde(default)]
    pub double_spend_pct: u8,
}

impl Default for ReorgJobRequest {
    fn default() -> Self {
        Self {
            depth: default_reorg_depth(),
            empty: false,
            node: default_reorg_node(),
            adds_new_txs: 0,
            double_spend_pct: 0,
        }
    }
}

fn default_reorg_depth() -> u64 {
    3
}

fn default_reorg_node() -> String {
    "node3".to_string()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobLease {
    pub component: String,
    pub lease_id: String,
    pub purpose: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobFailure {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobCleanup {
    pub state: CleanupState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl Default for JobCleanup {
    fn default() -> Self {
        Self {
            state: CleanupState::NotStarted,
            errors: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct JobSummary {
    pub id: String,
    pub kind: JobKind,
    pub state: JobState,
    pub phase: String,
    pub created_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    pub cleanup: JobCleanup,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct JobDetail {
    #[serde(flatten)]
    pub summary: JobSummary,
    pub request: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leases: Vec<JobLease>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<JobFailure>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobCreatedResponse {
    pub job_id: String,
    pub state: JobState,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reused: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct JobListResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_job_id: Option<String>,
    pub jobs: Vec<JobSummary>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct JobEvent {
    pub sequence: u64,
    pub job_id: String,
    pub timestamp_ms: u64,
    pub event: String,
    pub phase: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct JobEventsResponse {
    pub events: Vec<JobEvent>,
    pub next_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AbortJobResponse {
    pub job_id: String,
    pub state: JobState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_states_and_default_reorg_request_have_stable_json() {
        let request: ReorgJobRequest = serde_json::from_str("{}").expect("default request");
        assert_eq!(request, ReorgJobRequest::default());
        assert_eq!(
            serde_json::to_string(&JobState::AbortRequested).expect("state JSON"),
            "\"abort_requested\""
        );
        assert!(JobState::Interrupted.is_terminal());
        assert!(!JobState::Running.is_terminal());
    }
}
