//! Durable desired-state transaction: validate, apply to resident workers,
//! verify, persist, and restore the previous runtime policy on failure.

use crate::backend::{ChainBackend, MiningControlBackend, SpamControlBackend};
use crate::control_state;
use crate::control_state::{ControlState, ControlStateStore};
use crate::service::{
    config_error_details, validate_input, ErrorCode, ErrorDetail, RollbackReport, ServiceError,
};
use crate::state::AppState;
pub use simchain_common::control_api::{ApplyReport, ConfigPatchRequest as ApplyRequest};
use simchain_common::internal_api::{MiningWorkerStatus, SpamWorkerStatus, WorkerPhase};
use simchain_common::live_tuning::{self, LiveTuning, MiningTuning, ServiceScope, SpamTuning};
use std::collections::BTreeMap;
use std::sync::{Mutex, RwLock};
use std::time::Duration;

const STABILIZE_POLLS: u32 = 4;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct RuntimeSnapshot {
    mining: MiningWorkerStatus,
    spam: SpamWorkerStatus,
}

pub struct ApplyContext<'a> {
    pub apply_lock: &'a Mutex<()>,
    pub control_store: &'a ControlStateStore,
    pub control_state: &'a RwLock<ControlState>,
    pub chain: &'a dyn ChainBackend,
    pub mining: &'a dyn MiningControlBackend,
    pub spam: &'a dyn SpamControlBackend,
}

impl<'a> ApplyContext<'a> {
    pub fn from_app(app: &'a AppState) -> Self {
        Self {
            apply_lock: app.apply_lock.as_ref(),
            control_store: &app.control_store,
            control_state: app.control_state.as_ref(),
            chain: app.chain.as_ref(),
            mining: app.mining.as_ref(),
            spam: app.spam.as_ref(),
        }
    }
}

pub fn apply(app: &AppState, request: ApplyRequest) -> Result<ApplyReport, ServiceError> {
    let context = ApplyContext::from_app(app);
    apply_with_context(&context, request, |request| {
        app.jobs
            .ensure_idle()
            .map_err(crate::service::job_manager_error)?;
        if request.settings.contains_key("FALLBACK_FEE") && app.jobs.has_pending_faucet() {
            return Err(ServiceError::new(
                ErrorCode::FaucetDeliveryPending,
                "FALLBACK_FEE cannot change while a faucet transfer is armed",
            ));
        }
        Ok(())
    })
}

pub fn apply_with_context(
    context: &ApplyContext<'_>,
    request: ApplyRequest,
    preflight: impl FnOnce(&ApplyRequest) -> Result<(), ServiceError>,
) -> Result<ApplyReport, ServiceError> {
    let Ok(_process_guard) = context.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another apply is already in progress",
        ));
    };
    preflight(&request)?;
    let _file_guard = context
        .control_store
        .try_apply_lock()
        .map_err(|error| {
            ServiceError::new(
                ErrorCode::Internal,
                format!("cannot acquire the durable apply lock: {error}"),
            )
        })?
        .ok_or_else(|| {
            ServiceError::new(
                ErrorCode::ApplyInProgress,
                "another control-plane process holds the apply lock",
            )
        })?;

    let state_before = context.control_store.load_current().map_err(|error| {
        ServiceError::new(
            ErrorCode::Internal,
            format!("cannot reload durable desired state: {error}"),
        )
    })?;
    *context.control_state.write().expect("control state lock") = state_before.clone();
    if let Some(base_generation) = request.base_generation {
        if base_generation != state_before.generation {
            return Err(ServiceError::new(
                ErrorCode::StaleRevision,
                format!(
                    "the desired configuration changed since it was loaded (expected generation {base_generation}, current {})",
                    state_before.generation
                ),
            ));
        }
    }

    let proposed = validate_patch(&request.settings)?;
    let mut merged = state_before.desired.clone();
    merged.extend(proposed.clone());
    let source = live_tuning::staged_map(&merged);
    let (tuning, mut warnings) = parse_merged_tuning(&source)?;

    if proposed.contains_key("FALLBACK_FEE") {
        validate_dynamic_fee(context, tuning.spam.fallback_fee)?;
    }

    let desired = tuning
        .canonical_values()
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect::<BTreeMap<_, _>>();
    let next_state = control_state::successful_apply(&state_before, desired);
    let runtime_before = runtime_snapshot(context)?;
    let scopes = scopes_needing_apply(&runtime_before, &tuning, next_state.generation);

    if state_before.desired == next_state.desired && scopes.is_empty() {
        return Ok(ApplyReport {
            changed: false,
            components_applied: Vec::new(),
            generation: state_before.generation,
            logs: vec!["no-op: desired and effective worker policies already match".to_string()],
            warnings,
        });
    }

    let node1_reachable_before = context.chain.node1_height().is_ok();
    let mut logs = Vec::new();
    let mut attempted = Vec::new();
    for scope in &scopes {
        attempted.push(*scope);
        let result = match scope {
            ServiceScope::MiningController => context
                .mining
                .set_policy(next_state.generation, tuning.mining.clone()),
            ServiceScope::Spammer => context
                .spam
                .set_policy(next_state.generation, tuning.spam.clone()),
        };
        match result {
            Ok(ack) => logs.push(format!(
                "{} acknowledged generation {} in phase {}",
                scope.component_name(),
                ack.effective_generation,
                ack.phase.as_str()
            )),
            Err(error) => {
                let rollback = rollback_runtime(context, &attempted, &runtime_before, &mut logs);
                let code = if rollback.runtime_restored {
                    ErrorCode::ComponentUnavailable
                } else {
                    ErrorCode::RollbackFailed
                };
                let mut service_error = ServiceError::new(
                    code,
                    format!("{} rejected the policy: {error}", scope.component_name()),
                );
                service_error.rollback = Some(rollback);
                return Err(service_error);
            }
        }
    }

    if let Err(reason) = verify_runtime(
        context,
        &tuning,
        next_state.generation,
        &scopes,
        node1_reachable_before,
    ) {
        let rollback = rollback_runtime(context, &attempted, &runtime_before, &mut logs);
        let code = if rollback.runtime_restored {
            ErrorCode::ComponentUnavailable
        } else {
            ErrorCode::RollbackFailed
        };
        let mut service_error =
            ServiceError::new(code, format!("apply verification failed: {reason}"));
        service_error.rollback = Some(rollback);
        return Err(service_error);
    }
    if !scopes.is_empty() {
        logs.push("worker policy verification passed".to_string());
    }

    if let Err(error) = context.control_store.save(&next_state) {
        let rollback = rollback_runtime(context, &attempted, &runtime_before, &mut logs);
        let code = if rollback.runtime_restored {
            ErrorCode::Internal
        } else {
            ErrorCode::RollbackFailed
        };
        let mut service_error = ServiceError::new(
            code,
            format!("failed to persist desired configuration: {error}"),
        );
        service_error.rollback = Some(rollback);
        return Err(service_error);
    }
    *context.control_state.write().expect("control state lock") = next_state.clone();
    logs.push(format!(
        "persisted desired configuration generation {}",
        next_state.generation
    ));

    if !tuning.spam.enabled && scopes.contains(&ServiceScope::Spammer) {
        warnings.push(
            "spam is disabled: its resident worker remains reachable for status and live re-enable"
                .to_string(),
        );
    }

    Ok(ApplyReport {
        changed: true,
        components_applied: scopes
            .iter()
            .map(|scope| scope.component_name().to_string())
            .collect(),
        generation: next_state.generation,
        logs,
        warnings,
    })
}

fn validate_patch(
    settings: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, ServiceError> {
    let mut proposed = BTreeMap::new();
    let mut errors = Vec::new();
    for (key, value) in settings {
        match validate_input(key, value) {
            Ok(()) => {
                proposed.insert(key.clone(), value.trim().to_string());
            }
            Err(detail) => errors.push(detail),
        }
    }
    if errors.is_empty() {
        Ok(proposed)
    } else {
        Err(ServiceError::new(
            ErrorCode::ValidationFailed,
            "one or more proposed values are invalid",
        )
        .with_details(errors))
    }
}

fn validate_dynamic_fee(context: &ApplyContext<'_>, fallback_fee: f64) -> Result<(), ServiceError> {
    let required = context.chain.spam_min_fee().map_err(|error| {
        ServiceError::new(
            ErrorCode::RpcUnavailable,
            format!("cannot validate FALLBACK_FEE against the running nodes: {error}"),
        )
    })?;
    if fallback_fee + 1e-12 < required {
        return Err(ServiceError::new(
            ErrorCode::ValidationFailed,
            "FALLBACK_FEE is below a running node's relay/mempool minimum",
        )
        .with_details(vec![ErrorDetail {
            key: Some("FALLBACK_FEE".to_string()),
            value: Some(fallback_fee.to_string()),
            cause: format!("the highest active minimum is {required} BTC/kvB"),
        }]));
    }
    Ok(())
}

fn runtime_snapshot(context: &ApplyContext<'_>) -> Result<RuntimeSnapshot, ServiceError> {
    let mining = context.mining.status().map_err(|error| {
        ServiceError::new(
            ErrorCode::ComponentUnavailable,
            format!("mining worker is unavailable: {error}"),
        )
    })?;
    let spam = context.spam.status().map_err(|error| {
        ServiceError::new(
            ErrorCode::ComponentUnavailable,
            format!("spam worker is unavailable: {error}"),
        )
    })?;
    Ok(RuntimeSnapshot { mining, spam })
}

fn scopes_needing_apply(
    runtime: &RuntimeSnapshot,
    desired: &LiveTuning,
    generation: u64,
) -> Vec<ServiceScope> {
    let mut scopes = Vec::new();
    if runtime.mining.policy != desired.mining || runtime.mining.effective_generation != generation
    {
        scopes.push(ServiceScope::MiningController);
    }
    if runtime.spam.policy != desired.spam || runtime.spam.effective_generation != generation {
        scopes.push(ServiceScope::Spammer);
    }
    scopes
}

fn verify_runtime(
    context: &ApplyContext<'_>,
    desired: &LiveTuning,
    generation: u64,
    scopes: &[ServiceScope],
    node1_reachable_before: bool,
) -> Result<(), String> {
    for poll in 0..STABILIZE_POLLS {
        // Start after one full interval so four polls cover an approximately
        // eight-second post-acknowledgement stabilization window.
        context.chain.wait(POLL_INTERVAL);
        let last = poll + 1 == STABILIZE_POLLS;
        let mut errors = Vec::new();
        for scope in scopes {
            let result = match scope {
                ServiceScope::MiningController => context
                    .mining
                    .status()
                    .map_err(|error| error.to_string())
                    .and_then(|status| verify_mining(&status, desired, generation)),
                ServiceScope::Spammer => context
                    .spam
                    .status()
                    .map_err(|error| error.to_string())
                    .and_then(|status| verify_spam(&status, desired, generation)),
            };
            if let Err(error) = result {
                errors.push(format!("{}: {error}", scope.component_name()));
            }
        }
        if errors.is_empty() && last {
            if node1_reachable_before && context.chain.node1_height().is_err() {
                return Err("node1 RPC became unreachable after the apply".to_string());
            }
            return Ok(());
        }
        if !errors.is_empty() && last {
            return Err(errors.join("; "));
        }
    }
    unreachable!("the stabilization loop always returns")
}

fn verify_mining(
    status: &MiningWorkerStatus,
    desired: &LiveTuning,
    generation: u64,
) -> Result<(), String> {
    if status.phase == WorkerPhase::Error {
        return Err(status
            .last_error
            .clone()
            .unwrap_or_else(|| "worker entered the error phase".to_string()));
    }
    if status.effective_generation != generation {
        return Err(format!(
            "effective generation is {}, expected {generation}",
            status.effective_generation
        ));
    }
    if status.policy != desired.mining {
        return Err("effective policy does not match desired policy".to_string());
    }
    Ok(())
}

fn verify_spam(
    status: &SpamWorkerStatus,
    desired: &LiveTuning,
    generation: u64,
) -> Result<(), String> {
    if status.phase == WorkerPhase::Error {
        return Err(status
            .last_error
            .clone()
            .unwrap_or_else(|| "worker entered the error phase".to_string()));
    }
    if status.effective_generation != generation {
        return Err(format!(
            "effective generation is {}, expected {generation}",
            status.effective_generation
        ));
    }
    if status.policy != desired.spam {
        return Err("effective policy does not match desired policy".to_string());
    }
    if !desired.spam.enabled && status.phase != WorkerPhase::Disabled {
        return Err(format!(
            "disabled policy is active but worker phase is {}",
            status.phase.as_str()
        ));
    }
    Ok(())
}

fn rollback_runtime(
    context: &ApplyContext<'_>,
    scopes: &[ServiceScope],
    before: &RuntimeSnapshot,
    logs: &mut Vec<String>,
) -> RollbackReport {
    let mut messages = Vec::new();
    for scope in scopes {
        let result = match scope {
            ServiceScope::MiningController => context.mining.restore_policy(
                before.mining.effective_generation,
                before.mining.policy.clone(),
            ),
            ServiceScope::Spammer => context
                .spam
                .restore_policy(before.spam.effective_generation, before.spam.policy.clone()),
        };
        match result {
            Ok(_) => logs.push(format!(
                "rollback: restored {} generation",
                scope.component_name()
            )),
            Err(error) => messages.push(format!(
                "failed to restore {}: {error}",
                scope.component_name()
            )),
        }
    }

    let verified = verify_rollback(context, scopes, before).map_or_else(
        |error| {
            messages.push(error);
            false
        },
        |()| true,
    );
    RollbackReport {
        desired_state_preserved: true,
        runtime_restored: messages.is_empty() && verified,
        message: if messages.is_empty() {
            "restored the previous effective worker policies; durable desired state was unchanged"
                .to_string()
        } else {
            messages.join("; ")
        },
    }
}

fn verify_rollback(
    context: &ApplyContext<'_>,
    scopes: &[ServiceScope],
    before: &RuntimeSnapshot,
) -> Result<(), String> {
    for poll in 0..STABILIZE_POLLS {
        let last = poll + 1 == STABILIZE_POLLS;
        let mut errors = Vec::new();
        for scope in scopes {
            match scope {
                ServiceScope::MiningController => match context.mining.status() {
                    Ok(status)
                        if status.effective_generation == before.mining.effective_generation
                            && status.policy == before.mining.policy => {}
                    Ok(_) => errors.push("mining rollback policy has not converged".to_string()),
                    Err(error) => errors.push(format!("mining rollback status failed: {error}")),
                },
                ServiceScope::Spammer => match context.spam.status() {
                    Ok(status)
                        if status.effective_generation == before.spam.effective_generation
                            && status.policy == before.spam.policy => {}
                    Ok(_) => errors.push("spam rollback policy has not converged".to_string()),
                    Err(error) => errors.push(format!("spam rollback status failed: {error}")),
                },
            }
        }
        if errors.is_empty() && last {
            return Ok(());
        }
        if !errors.is_empty() && last {
            return Err(errors.join("; "));
        }
        context.chain.wait(POLL_INTERVAL);
    }
    unreachable!("the stabilization loop always returns")
}

/// If spam is being disabled, repair only invalid dormant spam values so an
/// operator can always return the resident worker to a reachable disabled
/// state. Mining validation remains strict.
fn parse_merged_tuning(
    merged: &BTreeMap<String, String>,
) -> Result<(LiveTuning, Vec<String>), ServiceError> {
    match LiveTuning::from_source(merged) {
        Ok(value) => Ok(value),
        Err(full_error) if !live_tuning::spam_enabled(merged) => {
            let mining = MiningTuning::from_source(merged).map_err(|error| {
                ServiceError::new(
                    ErrorCode::ValidationFailed,
                    "the merged mining configuration is invalid",
                )
                .with_details(config_error_details(&error))
            })?;
            let mut repaired = merged.clone();
            let defaults = live_tuning::staged_map(&BTreeMap::new());
            let mut reset_keys = Vec::new();
            let spam = loop {
                match SpamTuning::from_source(&repaired) {
                    Ok((spam, _)) => break spam,
                    Err(error) => {
                        let keys = config_error_details(&error)
                            .into_iter()
                            .filter_map(|detail| detail.key)
                            .filter(|key| {
                                live_tuning::spec(key).is_some_and(|spec| {
                                    spec.scope == ServiceScope::Spammer && key != "ENABLE_SPAM"
                                })
                            })
                            .collect::<Vec<_>>();
                        let mut progressed = false;
                        for key in keys {
                            let default = defaults.get(&key).cloned().unwrap_or_default();
                            if repaired.get(&key) != Some(&default) {
                                repaired.insert(key.clone(), default);
                                reset_keys.push(key);
                                progressed = true;
                            }
                        }
                        if !progressed
                            && repaired.get("SPAM_FANOUT_AUTO").map(String::as_str) != Some("true")
                        {
                            repaired.insert("SPAM_FANOUT_AUTO".to_string(), "true".to_string());
                            reset_keys.push("SPAM_FANOUT_AUTO".to_string());
                            progressed = true;
                        }
                        if !progressed {
                            return Err(ServiceError::new(
                                ErrorCode::ValidationFailed,
                                "disabled spam settings could not be canonicalized safely",
                            )
                            .with_details(config_error_details(&full_error)));
                        }
                    }
                }
            };
            reset_keys.sort();
            reset_keys.dedup();
            Ok((
                LiveTuning { mining, spam },
                vec![format!(
                    "ENABLE_SPAM=false: invalid dormant spam settings were reset to defaults ({})",
                    reset_keys.join(", ")
                )],
            ))
        }
        Err(error) => Err(ServiceError::new(
            ErrorCode::ValidationFailed,
            "the merged configuration is invalid",
        )
        .with_details(config_error_details(&error))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_state::{successful_apply, ControlState};
    use crate::state::{MINING_COMPONENT, SPAM_COMPONENT};
    use crate::test_support::{test_app, MockBackend};
    use std::sync::Arc;

    fn fixture() -> (tempfile::TempDir, Arc<MockBackend>, AppState) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new());
        mock.sync_workers();
        let app = test_app(dir.path(), mock.clone());
        (dir, mock, app)
    }

    fn request(entries: &[(&str, &str)], base_generation: Option<u64>) -> ApplyRequest {
        ApplyRequest {
            settings: entries
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
            base_generation,
        }
    }

    #[test]
    fn partial_update_advances_one_global_generation_and_persists_state() {
        let (dir, mock, app) = fixture();

        let report = apply(
            &app,
            request(&[("BLOCK_INTERVAL_MEAN_SECS", "12")], Some(0)),
        )
        .expect("apply");

        assert!(report.changed);
        assert_eq!(report.generation, 1);
        assert_eq!(
            report.components_applied,
            vec![MINING_COMPONENT.to_string(), SPAM_COMPONENT.to_string()]
        );
        assert_eq!(
            mock.policy_calls(),
            vec![
                (MINING_COMPONENT.to_string(), 1),
                (SPAM_COMPONENT.to_string(), 1)
            ]
        );
        let persisted = app.control_store.load_current().expect("persisted state");
        assert_eq!(persisted.generation, 1);
        assert_eq!(persisted.desired["BLOCK_INTERVAL_MEAN_SECS"], "12");
        assert!(!dir.path().join(".env").exists());
    }

    #[test]
    fn stale_generation_is_checked_against_fresh_durable_state() {
        let (_dir, mock, app) = fixture();
        let current = app.control_store.load_current().expect("current state");
        let mut desired = current.desired.clone();
        desired.insert("BLOCK_INTERVAL_MEAN_SECS".to_string(), "13".to_string());
        let newer = successful_apply(&current, desired);
        app.control_store.save(&newer).expect("external save");

        let error = apply(
            &app,
            request(&[("BLOCK_INTERVAL_MEAN_SECS", "12")], Some(0)),
        )
        .expect_err("stale apply");

        assert_eq!(error.code, ErrorCode::StaleRevision);
        assert!(mock.policy_calls().is_empty());
        assert_eq!(
            app.control_state.read().expect("control state").generation,
            1
        );
    }

    #[test]
    fn invalid_patch_is_rejected_before_worker_mutation() {
        let (_dir, mock, app) = fixture();

        let error =
            apply(&app, request(&[("ENABLE_SPAM", "1")], Some(0))).expect_err("invalid patch");

        assert_eq!(error.code, ErrorCode::ValidationFailed);
        assert!(mock.policy_calls().is_empty());
        assert_eq!(
            app.control_store
                .load_current()
                .expect("durable state")
                .generation,
            0
        );
    }

    #[test]
    fn unavailable_worker_prevents_any_partial_apply() {
        let (_dir, mock, app) = fixture();
        mock.set_worker_available(SPAM_COMPONENT, false);

        let error = apply(
            &app,
            request(&[("BLOCK_INTERVAL_MEAN_SECS", "12")], Some(0)),
        )
        .expect_err("unavailable worker");

        assert_eq!(error.code, ErrorCode::ComponentUnavailable);
        assert!(mock.policy_calls().is_empty());
        assert_eq!(
            app.control_store
                .load_current()
                .expect("durable state")
                .generation,
            0
        );
    }

    #[test]
    fn partial_worker_failure_restores_runtime_and_preserves_desired_state() {
        let (_dir, mock, app) = fixture();
        mock.world.lock().expect("world").spam_policy_fail_times = 1;

        let error = apply(
            &app,
            request(
                &[
                    ("BLOCK_INTERVAL_MEAN_SECS", "12"),
                    ("SPAM_FILL_BLOCK_RATIO", "3"),
                ],
                Some(0),
            ),
        )
        .expect_err("partial worker failure");

        assert_eq!(error.code, ErrorCode::ComponentUnavailable);
        let rollback = error.rollback.expect("rollback report");
        assert!(rollback.desired_state_preserved);
        assert!(rollback.runtime_restored);
        assert_eq!(app.mining.status().expect("mining").effective_generation, 0);
        assert_eq!(app.spam.status().expect("spam").effective_generation, 0);
        assert_eq!(
            app.control_store
                .load_current()
                .expect("durable state")
                .generation,
            0
        );
    }

    #[test]
    fn rollback_failure_is_reported_without_claiming_runtime_restoration() {
        let (_dir, mock, app) = fixture();
        {
            let mut world = mock.world.lock().expect("world");
            world.spam_policy_fail_times = 1;
            world.mining_restore_fail_times = 1;
        }

        let error = apply(
            &app,
            request(
                &[
                    ("BLOCK_INTERVAL_MEAN_SECS", "12"),
                    ("SPAM_FILL_BLOCK_RATIO", "3"),
                ],
                Some(0),
            ),
        )
        .expect_err("rollback failure");

        assert_eq!(error.code, ErrorCode::RollbackFailed);
        let rollback = error.rollback.expect("rollback report");
        assert!(rollback.desired_state_preserved);
        assert!(!rollback.runtime_restored);
        assert_eq!(
            app.control_store
                .load_current()
                .expect("durable state")
                .generation,
            0
        );
    }

    #[test]
    fn dynamic_fee_validation_happens_before_worker_mutation() {
        let (_dir, mock, app) = fixture();

        let error = apply(&app, request(&[("FALLBACK_FEE", "0.000001")], Some(0)))
            .expect_err("fee below runtime minimum");

        assert_eq!(error.code, ErrorCode::ValidationFailed);
        assert!(mock.policy_calls().is_empty());
    }

    #[test]
    fn node_rpc_postcondition_failure_rolls_back_worker_policies() {
        let (_dir, mock, app) = fixture();
        mock.world.lock().expect("world").kill_node1_on_policy = true;

        let error = apply(&app, request(&[("SPAM_FILL_BLOCK_RATIO", "3")], Some(0)))
            .expect_err("node postcondition failure");

        assert_eq!(error.code, ErrorCode::ComponentUnavailable);
        assert!(error.rollback.expect("rollback report").runtime_restored);
        assert_eq!(app.mining.status().expect("mining").effective_generation, 0);
        assert_eq!(app.spam.status().expect("spam").effective_generation, 0);
        assert_eq!(
            app.control_store
                .load_current()
                .expect("durable state")
                .generation,
            0
        );
    }

    #[test]
    fn runtime_drift_is_repaired_without_advancing_desired_generation() {
        let (_dir, mock, app) = fixture();
        mock.set_worker_policy_value(MINING_COMPONENT, "BLOCK_INTERVAL_MEAN_SECS", "12");

        let report = apply(&app, request(&[], Some(0))).expect("drift repair");

        assert!(report.changed);
        assert_eq!(report.generation, 0);
        assert_eq!(
            report.components_applied,
            vec![MINING_COMPONENT.to_string()]
        );
        assert_eq!(mock.policy_calls(), vec![(MINING_COMPONENT.to_string(), 0)]);
        assert_eq!(
            app.control_store
                .load_current()
                .expect("durable state")
                .generation,
            0
        );
    }

    #[test]
    fn default_control_state_matches_the_fixture_contract() {
        let (_dir, _mock, app) = fixture();
        assert_eq!(
            app.control_store.load_current().expect("state"),
            ControlState::default()
        );
    }
}
