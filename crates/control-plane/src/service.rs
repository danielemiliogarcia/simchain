//! Transport-agnostic operations layer: the HTTP handlers and the MCP tools
//! are both thin adapters over these functions, so the two surfaces cannot
//! drift. Shared response and error types live here too.

use crate::jobs::JobManagerError;
use crate::state::{AppState, MINING_COMPONENT, SPAM_COMPONENT};
use crate::status::StatusSnapshot;
use simchain_common::config::ConfigError;
use simchain_common::control_api::{
    AbortJobResponse, ApplyMode, BootSettingSchema, ComponentControlResponse, ConfigResponse,
    DegradeJobRequest, EffectiveComponentConfig, FaucetJobRequest, FaucetStatusResponse,
    FaucetTransfer, JobCheckpointResponse, JobCreatedResponse, JobDetail, JobEventsResponse,
    JobListResponse, MineJobRequest, OperationSummary, PartitionJobRequest,
    ReleaseCheckpointRequest, ReorgJobRequest, SchemaResponse, SettingSchema, SpamBurstJobRequest,
};
pub use simchain_common::control_api::{
    ApiError as ServiceError, ErrorCode, ErrorDetail, RollbackReport,
};
use simchain_common::internal_api::DesiredState;
use simchain_common::live_tuning::{self, ControlKind, LiveTuning, ServiceScope};
use std::collections::BTreeMap;

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
                component: spec.scope.component_name().to_string(),
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
                    ServiceScope::Spammer => spam_apply_mode(spec.key),
                },
                help: spec.help.to_string(),
                warning: spec.warning.map(str::to_string),
            })
            .collect(),
        boot_settings: vec![
            BootSettingSchema {
                key: "USE_RAW_TX_SPAM".to_string(),
                value: "true".to_string(),
                group: live_tuning::SettingGroup::SpamBasics.as_str().to_string(),
                note: "pinned · read-only".to_string(),
                help: "Always true: the raw engine signs spam locally and bypasses the node wallets. The node-wallet engine is deprecated and no longer selectable."
                    .to_string(),
            },
            BootSettingSchema {
                key: "FALLBACK_FEE".to_string(),
                value: boot_fallback_fee(),
                group: live_tuning::SettingGroup::SpamBasics.as_str().to_string(),
                note: "boot-time · read-only".to_string(),
                help: "The nodes' boot-time -fallbackfee (BTC/kvB): the wallet feerate when fee estimation has no data. Fixed until the node containers are recreated; the live spam fee is SPAM_FEE."
                    .to_string(),
            },
        ],
    }
}

/// The `-fallbackfee` the nodes booted with: docker-compose interpolates the
/// same `FALLBACK_FEE` variable into the node commands and this process's
/// environment. If the .env changed since the nodes started, this reflects
/// the current .env, not the running flag.
fn boot_fallback_fee() -> String {
    std::env::var("FALLBACK_FEE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "0.0001".to_string())
}

fn spam_apply_mode(key: &str) -> ApplyMode {
    match key {
        "ENABLE_SPAM"
        | "SPAM_FEE"
        | "SPAM_SENDMANY_OUTPUTS"
        | "SPAM_TX_DATA_MIN_BYTES"
        | "SPAM_TX_DATA_MAX_BYTES" => ApplyMode::EngineRebuild,
        _ => ApplyMode::NextSafePoint,
    }
}

fn validation_bounds(key: &str) -> (Option<f64>, Option<f64>) {
    match key {
        "BLOCK_INTERVAL_MEAN_SECS" => (Some(1.0), None),
        "BLOCK_INTERVAL_MIN_SECS" | "SPAM_FEE" | "SPAM_FILL_BLOCK_RATIO" => (Some(0.0), None),
        "BLOCK_INTERVAL_MAX_SECS" => (Some(f64::EPSILON), None),
        "SPAM_TX_DATA_MAX_BYTES" => (Some(0.0), Some(live_tuning::MAX_DATA_BYTES as f64)),
        key if live_tuning::spec(key).is_some_and(|spec| spec.control == ControlKind::Integer) => {
            (Some(0.0), None)
        }
        _ => (None, None),
    }
}

// ---------------------------------------------------------------------------
// Desired/effective configuration
// ---------------------------------------------------------------------------

pub fn config(app: &AppState) -> Result<ConfigResponse, ServiceError> {
    let control = load_durable_control_state(app)?;
    let (desired_valid, desired_errors, warnings, tuning) =
        match LiveTuning::from_source(&control.desired) {
            Ok((tuning, warnings)) => (true, Vec::new(), warnings, Some(tuning)),
            Err(error) => (false, config_error_details(&error), Vec::new(), None),
        };
    let mut pending_apply = Vec::new();
    let mut effective = BTreeMap::new();
    match app.mining.status() {
        Ok(status) => {
            if tuning.as_ref().is_none_or(|desired| {
                status.effective_generation != control.generation || status.policy != desired.mining
            }) {
                pending_apply.push(MINING_COMPONENT.to_string());
            }
            effective.insert(
                MINING_COMPONENT.to_string(),
                EffectiveComponentConfig {
                    reachable: true,
                    generation: Some(status.effective_generation),
                    values: Some(canonical_map(status.policy.canonical_values())),
                    error: status.last_error,
                },
            );
        }
        Err(error) => {
            pending_apply.push(MINING_COMPONENT.to_string());
            effective.insert(
                MINING_COMPONENT.to_string(),
                EffectiveComponentConfig {
                    reachable: false,
                    generation: None,
                    values: None,
                    error: Some(error.to_string()),
                },
            );
        }
    }
    match app.spam.status() {
        Ok(status) => {
            if tuning.as_ref().is_none_or(|desired| {
                status.effective_generation != control.generation || status.policy != desired.spam
            }) {
                pending_apply.push(SPAM_COMPONENT.to_string());
            }
            effective.insert(
                SPAM_COMPONENT.to_string(),
                EffectiveComponentConfig {
                    reachable: true,
                    generation: Some(status.effective_generation),
                    values: Some(canonical_map(status.policy.canonical_values())),
                    error: status.last_error,
                },
            );
        }
        Err(error) => {
            pending_apply.push(SPAM_COMPONENT.to_string());
            effective.insert(
                SPAM_COMPONENT.to_string(),
                EffectiveComponentConfig {
                    reachable: false,
                    generation: None,
                    values: None,
                    error: Some(error.to_string()),
                },
            );
        }
    }
    Ok(ConfigResponse {
        generation: control.generation,
        desired: control.desired,
        desired_valid,
        desired_errors,
        warnings,
        effective,
        pending_apply,
        components: app.status.read().expect("status lock").components.clone(),
    })
}

fn canonical_map(values: BTreeMap<&'static str, String>) -> BTreeMap<String, String> {
    values
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
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
    if let Some(mining) = status.components.get_mut(MINING_COMPONENT) {
        mining.desired_state = Some(control.mining_state);
    }
    if let Some(spam) = status.components.get_mut(SPAM_COMPONENT) {
        spam.desired_state = Some(control.spam_state);
    }
    status.active_operation = app.jobs.active_summary().map(|job| OperationSummary {
        job_id: job.id,
        kind: job.kind.as_str().to_string(),
        state: job.state.as_str().to_string(),
        phase: job.phase,
    });
    status
}

pub fn start_reorg(
    app: &std::sync::Arc<AppState>,
    request: ReorgJobRequest,
    idempotency_key: Option<String>,
) -> Result<JobCreatedResponse, ServiceError> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another desired-state mutation is already in progress",
        ));
    };
    let desired = load_durable_control_state(app)?.desired;
    let (tuning, _) = LiveTuning::from_source(&desired).map_err(|error| {
        ServiceError::new(
            ErrorCode::ValidationFailed,
            format!("durable spam policy is invalid: {error}"),
        )
    })?;
    app.jobs
        .start_reorg(request, idempotency_key, tuning.spam.use_raw)
        .map_err(job_manager_error)
}

pub fn start_faucet(
    app: &std::sync::Arc<AppState>,
    request: FaucetJobRequest,
    idempotency_key: Option<String>,
) -> Result<JobCreatedResponse, ServiceError> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another desired-state mutation is already in progress",
        ));
    };
    app.jobs
        .start_faucet(request, idempotency_key)
        .map_err(job_manager_error)
}

pub fn faucet_status(app: &AppState) -> FaucetStatusResponse {
    app.jobs.faucet_status()
}

pub fn faucet_transfer(app: &AppState, txid: &str) -> Result<FaucetTransfer, ServiceError> {
    app.jobs
        .faucet_transfer(txid)
        .ok_or_else(|| ServiceError::new(ErrorCode::JobNotFound, "faucet transfer not found"))
}

pub fn start_scenario(
    app: &std::sync::Arc<AppState>,
    yaml: String,
    idempotency_key: Option<String>,
) -> Result<JobCreatedResponse, ServiceError> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another desired-state mutation is already in progress",
        ));
    };
    let desired = load_durable_control_state(app)?.desired;
    let (tuning, _) = LiveTuning::from_source(&desired).map_err(|error| {
        ServiceError::new(
            ErrorCode::ValidationFailed,
            format!("durable spam policy is invalid: {error}"),
        )
    })?;
    app.jobs
        .start_scenario(yaml, idempotency_key, tuning.spam.use_raw)
        .map_err(job_manager_error)
}

pub fn start_mine(
    app: &std::sync::Arc<AppState>,
    request: MineJobRequest,
    idempotency_key: Option<String>,
) -> Result<JobCreatedResponse, ServiceError> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another desired-state mutation is already in progress",
        ));
    };
    app.jobs
        .start_mine(request, idempotency_key)
        .map_err(job_manager_error)
}

pub fn start_spam_burst(
    app: &std::sync::Arc<AppState>,
    request: SpamBurstJobRequest,
    idempotency_key: Option<String>,
) -> Result<JobCreatedResponse, ServiceError> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another desired-state mutation is already in progress",
        ));
    };
    app.jobs
        .start_spam_burst(request, idempotency_key)
        .map_err(job_manager_error)
}

pub fn start_partition(
    app: &std::sync::Arc<AppState>,
    request: PartitionJobRequest,
    idempotency_key: Option<String>,
) -> Result<JobCreatedResponse, ServiceError> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another desired-state mutation is already in progress",
        ));
    };
    app.jobs
        .start_partition(request, idempotency_key)
        .map_err(job_manager_error)
}

pub fn start_degrade(
    app: &std::sync::Arc<AppState>,
    request: DegradeJobRequest,
    idempotency_key: Option<String>,
) -> Result<JobCreatedResponse, ServiceError> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another desired-state mutation is already in progress",
        ));
    };
    app.jobs
        .start_degrade(request, idempotency_key)
        .map_err(job_manager_error)
}

pub fn list_jobs(app: &AppState) -> JobListResponse {
    app.jobs.list()
}

pub fn get_job(app: &AppState, job_id: &str) -> Result<JobDetail, ServiceError> {
    app.jobs.get(job_id).map_err(job_manager_error)
}

pub fn job_events(
    app: &AppState,
    job_id: Option<&str>,
    after: u64,
    limit: usize,
) -> Result<JobEventsResponse, ServiceError> {
    app.jobs
        .events(job_id, after, limit)
        .map_err(job_manager_error)
}

pub fn abort_job(app: &AppState, job_id: &str) -> Result<AbortJobResponse, ServiceError> {
    app.jobs.abort(job_id).map_err(job_manager_error)
}

pub fn get_checkpoint(
    app: &AppState,
    job_id: &str,
    name: &str,
) -> Result<JobCheckpointResponse, ServiceError> {
    app.jobs.checkpoint(job_id, name).map_err(job_manager_error)
}

pub fn release_checkpoint(
    app: &AppState,
    job_id: &str,
    name: &str,
    request: ReleaseCheckpointRequest,
) -> Result<JobCheckpointResponse, ServiceError> {
    app.jobs
        .release_checkpoint(job_id, name, request)
        .map_err(job_manager_error)
}

pub(crate) fn job_manager_error(error: JobManagerError) -> ServiceError {
    let mut service = ServiceError::new(error.code, error.message);
    if let Some(job_id) = error.active_job_id {
        service.details.push(ErrorDetail {
            key: Some("active_job_id".to_string()),
            value: Some(job_id),
            cause: "another chain-mutating job owns the coordinator".to_string(),
        });
    }
    service
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
    app.jobs.ensure_idle().map_err(job_manager_error)?;
    let _file_guard = durable_mutation_lock(app)?;
    let mut next = load_durable_control_state(app)?;
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

pub fn set_spam_state(
    app: &AppState,
    desired_state: DesiredState,
) -> Result<ComponentControlResponse, ServiceError> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another desired-state mutation is already in progress",
        ));
    };
    app.jobs.ensure_idle().map_err(job_manager_error)?;
    let _file_guard = durable_mutation_lock(app)?;
    let mut next = load_durable_control_state(app)?;
    next.spam_state = desired_state;
    app.control_store.save(&next).map_err(|error| {
        ServiceError::new(
            ErrorCode::Internal,
            format!("failed to persist desired spam state: {error}"),
        )
    })?;
    *app.control_state.write().expect("control state lock") = next;

    app.spam.set_state(desired_state).map_err(|error| {
        ServiceError::new(
            ErrorCode::ComponentUnavailable,
            format!("spam worker did not acknowledge the state change: {error}"),
        )
    })?;
    let status = app.spam.status().map_err(|error| {
        ServiceError::new(
            ErrorCode::ComponentUnavailable,
            format!("spam worker status is unavailable after state change: {error}"),
        )
    })?;
    Ok(ComponentControlResponse {
        component: "spam".to_string(),
        desired_state: status.desired_state,
        effective_state: status.effective_state,
        phase: status.phase,
        effective_generation: status.effective_generation,
    })
}

fn durable_mutation_lock(app: &AppState) -> Result<std::fs::File, ServiceError> {
    app.control_store
        .try_apply_lock()
        .map_err(|error| {
            ServiceError::new(
                ErrorCode::Internal,
                format!("cannot acquire the durable mutation lock: {error}"),
            )
        })?
        .ok_or_else(|| {
            ServiceError::new(
                ErrorCode::ApplyInProgress,
                "another control-plane process holds the mutation lock",
            )
        })
}

fn load_durable_control_state(
    app: &AppState,
) -> Result<crate::control_state::ControlState, ServiceError> {
    let state = app.control_store.load_current().map_err(|error| {
        ServiceError::new(
            ErrorCode::Internal,
            format!("cannot reload durable desired state: {error}"),
        )
    })?;
    *app.control_state.write().expect("control state lock") = state.clone();
    Ok(state)
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
        return Err(detail("not a runtime-managed setting".to_string()));
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

    #[test]
    fn validate_input_enforces_control_kinds() {
        assert!(validate_input("ENABLE_SPAM", "true").is_ok());
        assert!(validate_input("ENABLE_SPAM", "1").is_err());
        assert!(validate_input("BLOCK_INTERVAL_MODE", "poisson").is_ok());
        assert!(validate_input("BLOCK_INTERVAL_MODE", "gaussian").is_err());
        assert!(validate_input("SPAM_FLOOR_POOL_TXS", "100").is_ok());
        assert!(validate_input("SPAM_FLOOR_POOL_TXS", "-1").is_err());
        assert!(validate_input("SPAM_FEE", "0.0002").is_ok());
        assert!(validate_input("SPAM_FEE", "abc").is_err());
        // FALLBACK_FEE is boot-only and USE_RAW_TX_SPAM is pinned: neither is
        // a managed key, so neither is settable.
        assert!(validate_input("FALLBACK_FEE", "0.0002").is_err());
        assert!(validate_input("USE_RAW_TX_SPAM", "false").is_err());
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
            .find(|s| s.key == "SPAM_FEE")
            .expect("SPAM_FEE in schema");
        assert!(fee.warning.is_none(), "SPAM_FEE needs no warning banner");
        assert!(fee.help.contains("0.1 = 10,000 sat/vB"));
        assert!(fee.help.contains("capacity_degraded"));
        assert!(fee.help.contains("faucet reserve"));
        assert_eq!(fee.component, SPAM_COMPONENT);
        let boot_fee = view
            .boot_settings
            .iter()
            .find(|s| s.key == "FALLBACK_FEE")
            .expect("FALLBACK_FEE boot setting in schema");
        assert!(!boot_fee.value.is_empty());
        let engine = view
            .boot_settings
            .iter()
            .find(|s| s.key == "USE_RAW_TX_SPAM")
            .expect("pinned engine toggle in schema");
        assert_eq!(engine.value, "true");
        let cadence = view
            .settings
            .iter()
            .find(|setting| setting.key == "BLOCK_INTERVAL_MEAN_SECS")
            .expect("mining cadence in schema");
        assert_eq!(cadence.apply_mode, ApplyMode::NextSafePoint);
        let fill = view
            .settings
            .iter()
            .find(|setting| setting.key == "SPAM_FILL_BLOCK_RATIO")
            .expect("spam fill ratio in schema");
        assert_eq!(fill.apply_mode, ApplyMode::NextSafePoint);
        assert_eq!(fee.apply_mode, ApplyMode::EngineRebuild);
    }

    #[test]
    fn schema_classifies_every_runtime_setting() {
        let view = schema();
        let engine_rebuild = [
            "ENABLE_SPAM",
            "SPAM_FEE",
            "SPAM_SENDMANY_OUTPUTS",
            "SPAM_TX_DATA_MAX_BYTES",
            "SPAM_TX_DATA_MIN_BYTES",
        ];
        for spec in live_tuning::MANAGED_SETTINGS {
            let setting = view
                .settings
                .iter()
                .find(|setting| setting.key == spec.key)
                .unwrap_or_else(|| panic!("{} missing from schema", spec.key));
            assert_eq!(setting.component, spec.scope.component_name());
            let expected_mode = if engine_rebuild.contains(&spec.key) {
                ApplyMode::EngineRebuild
            } else {
                ApplyMode::NextSafePoint
            };
            assert_eq!(setting.apply_mode, expected_mode, "{}", spec.key);
        }
    }
}
