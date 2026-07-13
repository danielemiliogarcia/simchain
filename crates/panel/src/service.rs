//! Transport-agnostic operations layer: the HTTP handlers and the MCP tools
//! are both thin adapters over these functions, so the two surfaces cannot
//! drift. Shared response and error types live here too.

use crate::envfile;
use crate::state::{AppState, CONTROLLER_CONTAINER, SPAMMER_CONTAINER};
use crate::status::StatusSnapshot;
use serde::Serialize;
use simchain_common::config::ConfigError;
use simchain_common::live_tuning::{
    self, ControlKind, LiveTuning, MiningTuning, ServiceScope, SpamTuning,
};
use std::collections::{BTreeMap, HashMap};

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

/// Closed error-code enum: agents branch on codes, not prose.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ValidationFailed,
    StaleRevision,
    ApplyInProgress,
    ComposeFailed,
    RollbackFailed,
    /// Reserved in the stable API contract for read paths that require RPC;
    /// today every read serves the cached snapshot instead of failing.
    #[allow(dead_code)]
    RpcUnavailable,
    Unauthorized,
    Internal,
}

impl ErrorCode {
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::ValidationFailed => 422,
            ErrorCode::StaleRevision => 409,
            ErrorCode::ApplyInProgress => 409,
            ErrorCode::ComposeFailed => 500,
            ErrorCode::RollbackFailed => 500,
            ErrorCode::RpcUnavailable => 503,
            ErrorCode::Unauthorized => 401,
            ErrorCode::Internal => 500,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub cause: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RollbackReport {
    /// Whether `.env` was restored to its pre-apply contents.
    pub env_restored: bool,
    /// Whether the rollback recreate of the touched services succeeded.
    pub recreate_ok: bool,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServiceError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<ErrorDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback: Option<RollbackReport>,
}

impl ServiceError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Vec::new(),
            rollback: None,
        }
    }

    pub fn with_details(mut self, details: Vec<ErrorDetail>) -> Self {
        self.details = details;
        self
    }

    /// The JSON envelope shared by HTTP and MCP: `{"error": {...}}`.
    pub fn envelope(&self) -> serde_json::Value {
        serde_json::json!({ "error": self })
    }
}

pub fn config_error_details(error: &ConfigError) -> Vec<ErrorDetail> {
    match error {
        ConfigError::Aggregate(nested) => nested.iter().flat_map(config_error_details).collect(),
        ConfigError::Missing { key } => vec![ErrorDetail {
            key: Some((*key).to_string()),
            value: None,
            cause: "missing required value".to_string(),
        }],
        ConfigError::Invalid { key, value, cause }
        | ConfigError::OutOfRange { key, value, cause } => vec![ErrorDetail {
            key: Some((*key).to_string()),
            value: Some(value.clone()),
            cause: cause.clone(),
        }],
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct SettingSchema {
    pub key: &'static str,
    pub default: &'static str,
    pub group: &'static str,
    /// Compose service recreated when this key changes.
    pub scope: &'static str,
    pub control: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<&'static str>>,
    /// Empty input unsets optional settings and resets required settings to
    /// their compose default.
    pub optional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    pub help: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SchemaView {
    pub settings: Vec<SettingSchema>,
    pub legacy_aliases: Vec<&'static str>,
}

pub fn schema() -> SchemaView {
    SchemaView {
        settings: live_tuning::MANAGED_SETTINGS
            .iter()
            .map(|spec| SettingSchema {
                key: spec.key,
                default: spec.default,
                group: spec.group.as_str(),
                scope: spec.scope.service_name(),
                control: spec.control.as_str(),
                options: match spec.control {
                    ControlKind::Choice(options) => Some(options.to_vec()),
                    _ => None,
                },
                optional: spec.optional,
                minimum: validation_bounds(spec.key).0,
                maximum: validation_bounds(spec.key).1,
                help: spec.help,
                warning: spec.warning,
            })
            .collect(),
        legacy_aliases: live_tuning::LEGACY_SPAM_ALIASES.to_vec(),
    }
}

fn validation_bounds(key: &str) -> (Option<f64>, Option<f64>) {
    match key {
        "BLOCK_INTERVAL_MEAN_SECS" => (Some(1.0), None),
        "BLOCK_INTERVAL_MIN_SECS" | "FALLBACK_FEE" | "SPAM_FILL_BLOCK_RATIO" => (Some(0.0), None),
        "BLOCK_INTERVAL_MAX_SECS" => (Some(f64::EPSILON), None),
        "SPAM_TX_DATA_MAX_BYTES" => (Some(0.0), Some(live_tuning::MAX_DATA_BYTES as f64)),
        key if live_tuning::spec(key).is_some_and(|spec| spec.control == ControlKind::Integer) => {
            (Some(0.0), None)
        }
        _ => (None, None),
    }
}

// ---------------------------------------------------------------------------
// Settings state (staged vs running)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct RunningService {
    pub present: bool,
    /// Managed values (this service's scope) from the running container env.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SettingsStateView {
    pub revision: String,
    pub env_file_exists: bool,
    /// Full managed set: canonical when valid, raw staged values otherwise.
    pub staged: BTreeMap<String, String>,
    pub staged_valid: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub staged_errors: Vec<ErrorDetail>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub running: BTreeMap<String, RunningService>,
    /// Services whose running config differs from staged (what Apply would
    /// recreate right now).
    pub pending_restart: Vec<String>,
    pub services: BTreeMap<String, crate::status::ServiceState>,
}

/// Managed keys belonging to one service scope.
fn scope_keys(scope: ServiceScope) -> Vec<&'static str> {
    live_tuning::MANAGED_SETTINGS
        .iter()
        .filter(|spec| spec.scope == scope)
        .map(|spec| spec.key)
        .collect()
}

/// Does the staged tuning differ from what the container is running with?
/// Absent or unparsable containers count as "differs" (recreate fixes both).
pub fn scope_needs_restart(
    staged: &LiveTuning,
    running_env: Option<&HashMap<String, String>>,
    scope: ServiceScope,
) -> bool {
    let Some(env) = running_env else {
        return true;
    };
    match scope {
        ServiceScope::MiningController => match MiningTuning::from_source(env) {
            Ok(running) => running != staged.mining,
            Err(_) => true,
        },
        ServiceScope::Spammer => match SpamTuning::from_source(env) {
            Ok((running, _)) => running != staged.spam,
            Err(_) => true,
        },
    }
}

/// Parse the staged view out of the env-file contents.
pub struct Staged {
    pub overrides: BTreeMap<String, String>,
    pub tuning: Result<(LiveTuning, Vec<String>), ConfigError>,
}

pub fn tuning_source(overrides: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut source = live_tuning::staged_map(overrides);
    // Catalog defaults model compose, but must not shadow a recognized alias
    // when the canonical key was genuinely absent from the file.
    if !overrides.contains_key("SPAM_FIXED_TXS_PER_BLOCK")
        && ["SPAM_TXS_PER_BLOCK", "SPAM_PER_MINER_PER_BLOCK"]
            .iter()
            .any(|key| overrides.contains_key(*key))
    {
        source.remove("SPAM_FIXED_TXS_PER_BLOCK");
    }
    if !overrides.contains_key("SPAM_TX_DATA_MAX_BYTES")
        && overrides.contains_key("SPAM_TX_DATA_BYTES")
    {
        source.remove("SPAM_TX_DATA_MAX_BYTES");
    }
    for alias in live_tuning::LEGACY_SPAM_ALIASES {
        if let Some(value) = overrides.get(*alias) {
            source.insert((*alias).to_string(), value.clone());
        }
    }
    source
}

pub fn staged_from_content(content: &str) -> Staged {
    let overrides = envfile::managed_overrides(content);
    let tuning = LiveTuning::from_source(&tuning_source(&overrides));
    Staged { overrides, tuning }
}

pub fn settings_state(app: &AppState) -> Result<SettingsStateView, ServiceError> {
    let file = envfile::read_env_file(&app.config.env_file).map_err(|error| {
        ServiceError::new(
            ErrorCode::Internal,
            format!("failed to read env file: {error}"),
        )
    })?;

    let staged = staged_from_content(&file.content);
    let mut warnings: Vec<String> = envfile::legacy_aliases_present(&file.content)
        .into_iter()
        .map(|key| {
            format!(
                "{key} is a deprecated migration alias; its effective value is shown and the next successful apply will replace it with the canonical setting."
            )
        })
        .collect();

    let (staged_values, staged_valid, staged_errors, tuning) = match &staged.tuning {
        Ok((tuning, tuning_warnings)) => {
            warnings.extend(tuning_warnings.iter().cloned());
            (
                tuning
                    .canonical_values()
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                true,
                Vec::new(),
                Some(tuning.clone()),
            )
        }
        Err(error) => (
            live_tuning::staged_map(&staged.overrides),
            false,
            config_error_details(error),
            None,
        ),
    };

    let (inspected, inspect_error) = match app
        .executor
        .inspect(&[CONTROLLER_CONTAINER, SPAMMER_CONTAINER])
    {
        Ok(inspected) => (inspected, None),
        Err(error) => {
            let message = format!("docker inspect failed: {error}");
            warnings.push(message.clone());
            (HashMap::new(), Some(message))
        }
    };

    let mut running = BTreeMap::new();
    let mut pending_restart = Vec::new();
    for scope in [ServiceScope::MiningController, ServiceScope::Spammer] {
        let service = scope.service_name();
        let info = inspected.get(service);
        let values = info.map(|info| {
            scope_keys(scope)
                .into_iter()
                .map(|key| {
                    (
                        key.to_string(),
                        info.env.get(key).cloned().unwrap_or_default(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        });
        running.insert(
            service.to_string(),
            RunningService {
                present: info.is_some(),
                values,
                error: inspect_error.clone(),
            },
        );
        if inspect_error.is_none() {
            if let Some(tuning) = &tuning {
                if scope_needs_restart(tuning, info.map(|i| &i.env), scope) {
                    pending_restart.push(service.to_string());
                }
            }
        }
    }

    let services = app.status.read().expect("status lock").services.clone();

    Ok(SettingsStateView {
        revision: file.revision,
        env_file_exists: file.exists,
        staged: staged_values,
        staged_valid,
        staged_errors,
        warnings,
        running,
        pending_restart,
        services,
    })
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

pub fn status(app: &AppState) -> StatusSnapshot {
    app.status.read().expect("status lock").clone()
}

// ---------------------------------------------------------------------------
// Apply-input validation (strict, per key)
// ---------------------------------------------------------------------------

/// Strict per-key syntax check on proposed values. Stricter than the tools
/// themselves where tool semantics would surprise (ENABLE_SPAM=1 disables
/// spam, so bools accept only "true"/"false"); an empty value is always
/// allowed and means "unset" for optional settings or "reset to default" for
/// required settings. Range/cross-field validation
/// happens later on the merged set through the shared validators.
pub fn validate_input(key: &str, value: &str) -> Result<(), ErrorDetail> {
    let detail = |cause: String| ErrorDetail {
        key: Some(key.to_string()),
        value: Some(value.to_string()),
        cause,
    };
    let Some(spec) = live_tuning::spec(key) else {
        return Err(detail("not a panel-managed setting".to_string()));
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    match spec.control {
        ControlKind::Toggle => match trimmed {
            "true" | "false" => Ok(()),
            _ => Err(detail("expected true or false".to_string())),
        },
        ControlKind::Choice(options) => {
            if options.contains(&trimmed) {
                Ok(())
            } else {
                Err(detail(format!("expected one of: {}", options.join(", "))))
            }
        }
        ControlKind::Integer => trimmed
            .parse::<u64>()
            .map(|_| ())
            .map_err(|error| detail(format!("expected a non-negative integer ({error})"))),
        ControlKind::Decimal => match trimmed.parse::<f64>() {
            Ok(number) if number.is_finite() => Ok(()),
            Ok(_) => Err(detail("expected a finite number".to_string())),
            Err(error) => Err(detail(format!("expected a number ({error})"))),
        },
        ControlKind::Text => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CONTROLLER_CONTAINER, SPAMMER_CONTAINER};
    use crate::test_support::{test_app, MockExecutor};
    use std::sync::Arc;

    #[test]
    fn validate_input_enforces_control_kinds() {
        assert!(validate_input("ENABLE_SPAM", "true").is_ok());
        assert!(validate_input("ENABLE_SPAM", "1").is_err());
        assert!(validate_input("BLOCK_INTERVAL_MODE", "poisson").is_ok());
        assert!(validate_input("BLOCK_INTERVAL_MODE", "gaussian").is_err());
        assert!(validate_input("SPAM_FLOOR_POOL_TXS", "100").is_ok());
        assert!(validate_input("SPAM_FLOOR_POOL_TXS", "-1").is_err());
        assert!(validate_input("FALLBACK_FEE", "0.0002").is_ok());
        assert!(validate_input("FALLBACK_FEE", "abc").is_err());
        assert!(validate_input("MINER_WEIGHTS", "70,30").is_ok());
        assert!(validate_input("NOT_A_KEY", "1").is_err());
        // Empty always means "reset to default".
        assert!(validate_input("ENABLE_SPAM", "").is_ok());
    }

    #[test]
    fn schema_covers_the_whole_catalog() {
        let view = schema();
        assert_eq!(view.settings.len(), live_tuning::MANAGED_SETTINGS.len());
        let fee = view
            .settings
            .iter()
            .find(|s| s.key == "FALLBACK_FEE")
            .expect("FALLBACK_FEE in schema");
        assert!(fee.warning.is_some(), "node-restart caveat must be visible");
        assert_eq!(fee.scope, SPAMMER_CONTAINER);
    }

    #[test]
    fn settings_state_reports_drift_and_legacy_aliases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "SPAM_TXS_PER_BLOCK=500\nFALLBACK_FEE=0.0002\n")
            .expect("seed env");
        let mock = Arc::new(MockExecutor::new(env_file));
        mock.sync_containers();
        // Make the spammer run with an older fee than staged.
        mock.set_container_env(SPAMMER_CONTAINER, "FALLBACK_FEE", "0.0001");
        let app = test_app(dir.path(), mock);

        let view = settings_state(&app).expect("state");
        assert!(view.staged_valid);
        assert_eq!(view.staged["FALLBACK_FEE"], "0.0002");
        // Legacy alias participates in the effective staged configuration and
        // is surfaced as a migration warning.
        assert_eq!(view.staged["SPAM_FIXED_TXS_PER_BLOCK"], "500");
        assert!(view
            .warnings
            .iter()
            .any(|w| w.contains("SPAM_TXS_PER_BLOCK")));
        assert_eq!(view.pending_restart, vec![SPAMMER_CONTAINER]);
        assert!(view.running[CONTROLLER_CONTAINER].present);
    }

    #[test]
    fn settings_state_surfaces_invalid_staged_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "MINER_WEIGHTS=0,0\n").expect("seed env");
        let mock = Arc::new(MockExecutor::new(env_file));
        mock.sync_containers();
        let app = test_app(dir.path(), mock);

        let view = settings_state(&app).expect("state");
        assert!(!view.staged_valid);
        assert!(view
            .staged_errors
            .iter()
            .any(|d| d.key.as_deref() == Some("MINER_WEIGHTS")));
        // Raw staged values are still shown so the user can fix them.
        assert_eq!(view.staged["MINER_WEIGHTS"], "0,0");
    }

    #[test]
    fn missing_env_file_loads_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockExecutor::new(dir.path().join(".env")));
        mock.sync_containers();
        let app = test_app(dir.path(), mock);

        let view = settings_state(&app).expect("state");
        assert!(!view.env_file_exists);
        assert_eq!(view.revision, crate::envfile::ABSENT_REVISION);
        assert!(view.staged_valid);
        assert_eq!(view.staged["BLOCK_INTERVAL_MEAN_SECS"], "15");
        assert!(view.pending_restart.is_empty());
    }
}
