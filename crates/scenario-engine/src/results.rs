use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScenarioStepResult {
    pub index: usize,
    pub kind: String,
    pub duration_ms: u64,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScenarioResult {
    pub success: bool,
    pub aborted: bool,
    pub executed_steps: usize,
    pub total_steps: usize,
    pub duration_ms: u64,
    pub steps: Vec<ScenarioStepResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_summary: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
