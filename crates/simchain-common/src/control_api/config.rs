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
    /// Phase-1 compatibility adapter. Removed with Compose control.
    LegacyRecreate,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legacy_aliases: Vec<String>,
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
    /// Transitional CAS value for the legacy `.env` adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_revision: Option<String>,
    #[serde(default)]
    pub legacy_env_file_exists: bool,
}
