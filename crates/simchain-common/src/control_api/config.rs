use super::{ComponentState, ErrorDetail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyMode {
    Immediate,
    NextSafePoint,
    EngineRebuild,
    BootOnly,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SettingSchema {
    pub key: String,
    pub default: String,
    pub group: String,
    pub component: String,
    pub control: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    pub apply_mode: ApplyMode,
    pub help: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SchemaResponse {
    pub settings: Vec<SettingSchema>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ConfigPatchRequest {
    /// Partial desired-state update. An empty value resets a setting to its
    /// default, or unsets an optional setting.
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// Optional compare-and-swap guard against a stale editor.
    #[serde(default)]
    pub base_generation: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ApplyReport {
    pub changed: bool,
    pub components_applied: Vec<String>,
    pub generation: u64,
    pub logs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectiveComponentConfig {
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ConfigResponse {
    pub generation: u64,
    pub desired: BTreeMap<String, String>,
    pub desired_valid: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub desired_errors: Vec<ErrorDetail>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub effective: BTreeMap<String, EffectiveComponentConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_apply: Vec<String>,
    pub components: BTreeMap<String, ComponentState>,
}
