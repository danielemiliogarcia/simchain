//! Transport-agnostic operations layer: the HTTP handlers and the MCP tools
//! are both thin adapters over these functions, so the two surfaces cannot
//! drift. Shared response and error types live here too.

use crate::envfile;
use crate::state::{AppState, CONTROLLER_CONTAINER, SPAMMER_CONTAINER};
use crate::status::StatusSnapshot;
use serde::Serialize;
use simchain_common::config::ConfigError;
pub use simchain_common::control_api::{
    ApiError as ServiceError, ErrorCode, ErrorDetail, RollbackReport,
};
use simchain_common::control_api::{
    ApplyMode, ComponentControlResponse, ConfigResponse, EffectiveComponentConfig, SchemaResponse,
    SettingSchema,
};
use simchain_common::internal_api::DesiredState;
use simchain_common::live_tuning::{
    self, ControlKind, LiveTuning, MiningTuning, ServiceScope, SpamTuning,
};
use std::collections::{BTreeMap, HashMap};

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

pub fn schema() -> SchemaResponse {
    SchemaResponse {
        settings: live_tuning::MANAGED_SETTINGS
            .iter()
            .map(|spec| SettingSchema {
                key: spec.key.to_string(),
                default: spec.default.to_string(),
                group: spec.group.as_str().to_string(),
                component: spec.scope.service_name().to_string(),
                control: spec.control.as_str().to_string(),
                options: match spec.control {
                    ControlKind::Choice(options) => {
                        Some(options.iter().map(|option| (*option).to_string()).collect())
                    }
                    _ => None,
                },
                optional: spec.optional,
                minimum: validation_bounds(spec.key).0,
                maximum: validation_bounds(spec.key).1,
                apply_mode: match spec.scope {
                    ServiceScope::MiningController => ApplyMode::NextSafePoint,
                    ServiceScope::Spammer => ApplyMode::LegacyRecreate,
                },
                help: spec.help.to_string(),
                warning: spec.warning.map(str::to_string),
            })
            .collect(),
        legacy_aliases: live_tuning::LEGACY_SPAM_ALIASES
            .iter()
            .map(|alias| (*alias).to_string())
            .collect(),
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
    pub generation: Option<u64>,
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
    pub services: BTreeMap<String, simchain_common::control_api::ComponentState>,
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
        .components
        .inspect_components(&[CONTROLLER_CONTAINER, SPAMMER_CONTAINER])
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
                        info.effective_config.get(key).cloned().unwrap_or_default(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        });
        running.insert(
            service.to_string(),
            RunningService {
                present: info.is_some_and(|component| component.present),
                values,
                generation: info.and_then(|component| component.effective_generation),
                error: inspect_error
                    .clone()
                    .or_else(|| info.and_then(|component| component.last_error.clone())),
            },
        );
        if inspect_error.is_none() {
            if let Some(tuning) = &tuning {
                if scope_needs_restart(tuning, info.map(|i| &i.effective_config), scope) {
                    pending_restart.push(service.to_string());
                }
            }
        }
    }

    let services = app.status.read().expect("status lock").components.clone();

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

/// Stable v1 configuration response. During Phase 1 `desired` is still
/// sourced from the legacy env adapter and `effective` from component
/// inspection; consumers do not need to know which backend supplied either.
pub fn config(app: &AppState) -> Result<ConfigResponse, ServiceError> {
    let legacy = settings_state(app)?;
    let control = app
        .control_state
        .read()
        .expect("control state lock")
        .clone();
    let (desired_valid, desired_errors, mut warnings, tuning) =
        match LiveTuning::from_source(&control.desired) {
            Ok((tuning, warnings)) => (true, Vec::new(), warnings, Some(tuning)),
            Err(error) => (false, config_error_details(&error), Vec::new(), None),
        };
    warnings.extend(legacy.warnings);
    let mut pending_apply = Vec::new();
    if let Some(tuning) = &tuning {
        for scope in [ServiceScope::MiningController, ServiceScope::Spammer] {
            let component = scope.service_name();
            let running = legacy.running.get(component);
            let values = running
                .and_then(|running| running.values.as_ref())
                .map(|values| {
                    values
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<HashMap<_, _>>()
                });
            if running.is_none_or(|running| !running.present || running.error.is_some())
                || scope_needs_restart(tuning, values.as_ref(), scope)
            {
                pending_apply.push(component.to_string());
            }
        }
    }
    let effective = legacy
        .running
        .into_iter()
        .map(|(component, running)| {
            (
                component,
                EffectiveComponentConfig {
                    reachable: running.present && running.error.is_none(),
                    generation: running.generation,
                    values: running.values,
                    error: running.error,
                },
            )
        })
        .collect();
    Ok(ConfigResponse {
        generation: control.generation,
        desired: control.desired,
        desired_valid,
        desired_errors,
        warnings,
        effective,
        pending_apply,
        components: legacy.services,
        legacy_revision: Some(legacy.revision),
        legacy_env_file_exists: legacy.env_file_exists,
    })
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

pub fn status(app: &AppState) -> StatusSnapshot {
    let mut status = app.status.read().expect("status lock").clone();
    let control = app
        .control_state
        .read()
        .expect("control state lock")
        .clone();
    status.desired_generation = control.generation;
    if let Some(mining) = status.components.get_mut(CONTROLLER_CONTAINER) {
        mining.desired_state = Some(control.mining_state);
    }
    status
}

pub fn set_mining_state(
    app: &AppState,
    desired_state: DesiredState,
) -> Result<ComponentControlResponse, ServiceError> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another desired-state mutation is already in progress",
        ));
    };
    let mut next = app
        .control_state
        .read()
        .expect("control state lock")
        .clone();
    next.mining_state = desired_state;
    app.control_store.save(&next).map_err(|error| {
        ServiceError::new(
            ErrorCode::Internal,
            format!("failed to persist desired mining state: {error}"),
        )
    })?;
    *app.control_state.write().expect("control state lock") = next;

    app.mining.set_state(desired_state).map_err(|error| {
        ServiceError::new(
            ErrorCode::ComponentUnavailable,
            format!("mining worker did not acknowledge the state change: {error}"),
        )
    })?;
    let status = app.mining.status().map_err(|error| {
        ServiceError::new(
            ErrorCode::ComponentUnavailable,
            format!("mining worker status is unavailable after state change: {error}"),
        )
    })?;
    Ok(ComponentControlResponse {
        component: "mining".to_string(),
        desired_state: status.desired_state,
        effective_state: status.effective_state,
        phase: status.phase,
        effective_generation: status.effective_generation,
    })
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
    use crate::test_support::{test_app, MockBackend};
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
        assert_eq!(fee.component, SPAMMER_CONTAINER);
        let cadence = view
            .settings
            .iter()
            .find(|setting| setting.key == "BLOCK_INTERVAL_MEAN_SECS")
            .expect("mining cadence in schema");
        assert_eq!(cadence.apply_mode, ApplyMode::NextSafePoint);
    }

    #[test]
    fn settings_state_reports_drift_and_legacy_aliases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "SPAM_TXS_PER_BLOCK=500\nFALLBACK_FEE=0.0002\n")
            .expect("seed env");
        let mock = Arc::new(MockBackend::new(env_file));
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
        let mock = Arc::new(MockBackend::new(env_file));
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
        let mock = Arc::new(MockBackend::new(dir.path().join(".env")));
        mock.sync_containers();
        let app = test_app(dir.path(), mock);

        let view = settings_state(&app).expect("state");
        assert!(!view.env_file_exists);
        assert_eq!(view.revision, crate::envfile::ABSENT_REVISION);
        assert!(view.staged_valid);
        assert_eq!(view.staged["BLOCK_INTERVAL_MEAN_SECS"], "15");
        assert!(view.pending_restart.is_empty());
    }

    #[test]
    fn public_config_uses_durable_desired_state_after_initialization() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        let mock = Arc::new(MockBackend::new(env_file.clone()));
        mock.sync_containers();
        let app = test_app(dir.path(), mock);

        std::fs::write(&env_file, "BLOCK_INTERVAL_MEAN_SECS=99\n").expect("external legacy edit");
        let view = config(&app).expect("config");
        assert_eq!(view.desired["BLOCK_INTERVAL_MEAN_SECS"], "15");
        assert_eq!(view.effective[CONTROLLER_CONTAINER].generation, Some(0));
    }
}
