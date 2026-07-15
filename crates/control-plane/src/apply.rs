//! The apply transaction: validate -> persist compatibility state -> apply
//! through resident workers -> verify -> rollback. Everything here is
//! blocking; callers run it in `spawn_blocking`.

use crate::backend::ComponentInfo;
use crate::control_state;
use crate::envfile::{self, EnvFileState};
use crate::service::{
    config_error_details, scope_needs_restart, tuning_source, validate_input, ErrorCode,
    ErrorDetail, RollbackReport, ServiceError,
};
use crate::state::AppState;
use fs2::FileExt;
use serde::Serialize;
use simchain_common::live_tuning::{self, LiveTuning, MiningTuning, ServiceScope, SpamTuning};
use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::time::Duration;

/// Post-apply stabilization: 4 polls, 2s apart (~8s window), catching a
/// worker that acknowledged policy installation and then failed.
const STABILIZE_POLLS: u32 = 4;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct ApplyRequest {
    /// Partial: only the keys to change; empty unsets optional settings and
    /// resets required settings to their compose default.
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// When present and stale the apply is rejected with `stale_revision`;
    /// when absent the merge runs against whatever is current.
    #[serde(default)]
    pub base_revision: Option<String>,
    /// Stable control-plane generation used by `PATCH /api/v1/config`.
    #[serde(default)]
    pub base_generation: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyReport {
    /// False when neither the file nor any service needed changing.
    pub changed: bool,
    pub file_changed: bool,
    /// Components whose runtime policy was changed. This is transport
    /// neutral: both workers apply without process recreation.
    pub components_applied: Vec<String>,
    /// Retained for compatibility with Phase-1 clients; empty after both
    /// runtime workers migrated.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub legacy_services_recreated: Vec<String>,
    pub revision: String,
    pub generation: u64,
    pub logs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

pub fn apply(app: &AppState, request: ApplyRequest) -> Result<ApplyReport, ServiceError> {
    // In-process serialization first, then a kernel flock so a second control-plane
    // process cannot race the same .env; the kernel releases the flock if
    // this process dies mid-apply.
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another apply is already in progress",
        ));
    };
    app.jobs
        .ensure_idle()
        .map_err(crate::service::job_manager_error)?;
    let lock_path = app.config.env_file.with_file_name(format!(
        "{}.panel.lock",
        app.config
            .env_file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".env".to_string())
    ));
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            ServiceError::new(
                ErrorCode::Internal,
                format!("cannot open lock file: {error}"),
            )
        })?;
    if lock_file.try_lock_exclusive().is_err() {
        return Err(ServiceError::new(
            ErrorCode::ApplyInProgress,
            "another control-plane process holds the apply lock",
        ));
    }
    // Keep the bind-mounted lock file owned by the host user, like .env.
    if let Some(dir) = app.config.env_file.parent() {
        if let Ok(ownership) = envfile::dir_ownership(dir, 0o644) {
            let _ = std::os::unix::fs::chown(&lock_path, Some(ownership.uid), Some(ownership.gid));
        }
    }

    let mut logs: Vec<String> = Vec::new();

    // Re-read under the lock and check the caller's revision.
    let file = envfile::read_env_file(&app.config.env_file).map_err(|error| {
        ServiceError::new(
            ErrorCode::Internal,
            format!("failed to read env file: {error}"),
        )
    })?;
    if let Some(base_revision) = &request.base_revision {
        if *base_revision != file.revision {
            return Err(ServiceError::new(
                ErrorCode::StaleRevision,
                format!(
                    "the env file changed since this form was loaded (expected revision {base_revision}, current {})",
                    file.revision
                ),
            ));
        }
    }
    let state_before = app
        .control_state
        .read()
        .expect("control state lock")
        .clone();
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

    // Strict per-key input validation before any merge.
    let mut input_errors: Vec<ErrorDetail> = Vec::new();
    let mut proposed: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in &request.settings {
        match validate_input(key, value) {
            Ok(()) => {
                proposed.insert(key.clone(), value.trim().to_string());
            }
            Err(detail) => input_errors.push(detail),
        }
    }
    if !input_errors.is_empty() {
        return Err(ServiceError::new(
            ErrorCode::ValidationFailed,
            "one or more proposed values are invalid",
        )
        .with_details(input_errors));
    }

    // Durable control state is authoritative once initialized. The legacy
    // env file remains a compatibility mirror until Phase 7 removes it.
    let mut merged_map = state_before.desired.clone();
    for (key, value) in &proposed {
        // Keep an explicit empty marker so reset-to-default cannot fall back
        // to a legacy alias during source expansion.
        merged_map.insert(key.clone(), value.clone());
        if key == "SPAM_FIXED_TXS_PER_BLOCK" {
            merged_map.remove("SPAM_TXS_PER_BLOCK");
            merged_map.remove("SPAM_PER_MINER_PER_BLOCK");
        } else if key == "SPAM_TX_DATA_MAX_BYTES" {
            merged_map.remove("SPAM_TX_DATA_BYTES");
        }
    }
    let merged_map = tuning_source(&merged_map);
    let (tuning, mut warnings) = parse_merged_tuning(&merged_map)?;

    // Spam is submitted through node2/node3 and relayed across the network.
    // Validate against the strongest current relay or dynamic mempool floor;
    // failing open here would accept a setting whose transactions are then
    // rejected after the recreate.
    if proposed.contains_key("FALLBACK_FEE") {
        let required = app.job_actions.spam_min_fee().map_err(|error| {
            ServiceError::new(
                ErrorCode::RpcUnavailable,
                format!("cannot validate FALLBACK_FEE against the running nodes: {error}"),
            )
        })?;
        if tuning.spam.fallback_fee + 1e-12 < required {
            return Err(ServiceError::new(
                ErrorCode::ValidationFailed,
                "FALLBACK_FEE is below a running node's relay/mempool minimum",
            )
            .with_details(vec![ErrorDetail {
                key: Some("FALLBACK_FEE".to_string()),
                value: Some(tuning.spam.fallback_fee.to_string()),
                cause: format!("the highest active minimum is {required} BTC/kvB"),
            }]));
        }
    }

    // Two independent change sets (finding 6): the file diff decides the
    // compatibility write, while effective worker policy decides runtime work.
    let canonical = tuning.canonical_values();
    let desired: BTreeMap<String, String> = canonical
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect();
    let state_changed = desired != state_before.desired;
    let next_state = control_state::successful_apply(&state_before, desired.clone());
    let new_content = envfile::render_with_managed_block(&file.content, &canonical);
    let file_changed = !file.exists || new_content != file.content;

    let inspected = app
        .components
        .inspect_components(&[
            ServiceScope::MiningController.service_name(),
            ServiceScope::Spammer.service_name(),
        ])
        .map_err(|error| {
            ServiceError::new(
                ErrorCode::Internal,
                format!("component inspection failed: {error}"),
            )
        })?;
    let runtime_before = inspected.clone();
    let mut services: Vec<String> = Vec::new();
    for scope in [ServiceScope::MiningController, ServiceScope::Spammer] {
        let env = inspected
            .get(scope.service_name())
            .map(|info| &info.effective_config);
        if scope_needs_restart(&tuning, env, scope) {
            services.push(scope.service_name().to_string());
        }
    }

    for service in &services {
        if inspected
            .get(service)
            .is_none_or(|component| !component.present)
        {
            let detail = inspected
                .get(service)
                .and_then(|component| component.last_error.as_deref())
                .unwrap_or("worker status is unavailable");
            return Err(ServiceError::new(
                ErrorCode::ComponentUnavailable,
                format!("{service} worker is unavailable: {detail}"),
            ));
        }
    }

    if !file_changed && services.is_empty() && !state_changed {
        return Ok(ApplyReport {
            changed: false,
            file_changed: false,
            components_applied: Vec::new(),
            legacy_services_recreated: Vec::new(),
            revision: file.revision,
            generation: state_before.generation,
            logs: vec!["no-op: staged file and running services already match".to_string()],
            warnings,
        });
    }

    let node1_reachable_before = app.job_actions.node1_height().is_ok();

    if file_changed {
        let ownership = match file.ownership {
            Some(ownership) => ownership,
            None => {
                let dir = app
                    .config
                    .env_file
                    .parent()
                    .unwrap_or(std::path::Path::new("."));
                envfile::dir_ownership(dir, 0o644).map_err(|error| {
                    ServiceError::new(
                        ErrorCode::Internal,
                        format!("cannot stat env dir for ownership: {error}"),
                    )
                })?
            }
        };
        envfile::write_atomic(&app.config.env_file, &new_content, ownership).map_err(|error| {
            ServiceError::new(
                ErrorCode::Internal,
                format!("failed to write env file: {error}"),
            )
        })?;
        logs.push(format!(
            "wrote {} ({} managed keys canonicalized)",
            app.config.env_file.display(),
            canonical.len()
        ));
    }
    let written_revision = file_changed.then(|| envfile::revision_of(&new_content));

    if !services.is_empty() {
        let applied =
            app.configuration
                .apply_configuration(&services, &desired, next_state.generation);
        let apply_failed = match &applied {
            Ok(output) if output.success => {
                logs.push(format!("applied: {}", services.join(", ")));
                let tail = output.tail(5);
                if !tail.is_empty() {
                    logs.push(tail);
                }
                None
            }
            Ok(output) => Some(format!("component apply failed: {}", output.tail(10))),
            Err(error) => Some(format!("component apply failed: {error}")),
        };
        if let Some(reason) = apply_failed {
            let rollback = rollback(
                app,
                &file,
                written_revision.as_deref(),
                &services,
                &runtime_before,
                &mut logs,
            );
            let code = failure_code(&services, &rollback);
            let mut error = ServiceError::new(code, reason);
            error.rollback = Some(rollback);
            return Err(error);
        }

        if let Err(reason) = verify(app, &tuning, &services, node1_reachable_before) {
            let rollback = rollback(
                app,
                &file,
                written_revision.as_deref(),
                &services,
                &runtime_before,
                &mut logs,
            );
            let code = failure_code(&services, &rollback);
            let mut error = ServiceError::new(code, format!("apply verification failed: {reason}"));
            error.rollback = Some(rollback);
            return Err(error);
        }
        logs.push("verification passed".to_string());
    }

    // A manual writer does not honor our control-plane flock. Detect one after the
    // slow apply/verification window instead of returning a revision that
    // is already stale. Roll runtime back, but never overwrite that newer
    // external file in the rollback CAS.
    let current = envfile::read_env_file(&app.config.env_file).map_err(|error| {
        ServiceError::new(
            ErrorCode::Internal,
            format!("failed to re-read env file after apply: {error}"),
        )
    })?;
    let expected_revision = written_revision.as_ref().unwrap_or(&file.revision);
    if current.revision != *expected_revision {
        let rollback = rollback(
            app,
            &file,
            written_revision.as_deref(),
            &services,
            &runtime_before,
            &mut logs,
        );
        let mut error = ServiceError::new(
            ErrorCode::StaleRevision,
            "the env file was edited outside the control plane while apply was running; the newer file was preserved and runtime changes were rolled back",
        );
        error.rollback = Some(rollback);
        return Err(error);
    }
    let revision = current.revision;
    if !tuning.spam.enabled
        && services
            .iter()
            .any(|s| s == ServiceScope::Spammer.service_name())
    {
        warnings.push(
            "spam is disabled: the resident worker remains available for status and runtime re-enable"
                .to_string(),
        );
    }

    if let Err(error) = app.control_store.save(&next_state) {
        let rollback = rollback(
            app,
            &file,
            written_revision.as_deref(),
            &services,
            &runtime_before,
            &mut logs,
        );
        let mut service_error = ServiceError::new(
            ErrorCode::RollbackFailed,
            format!("failed to persist control state: {error}"),
        );
        service_error.rollback = Some(rollback);
        return Err(service_error);
    }
    *app.control_state.write().expect("control state lock") = next_state.clone();

    Ok(ApplyReport {
        changed: true,
        file_changed,
        legacy_services_recreated: Vec::new(),
        components_applied: services,
        revision,
        generation: next_state.generation,
        logs,
        warnings,
    })
}

fn failure_code(_components: &[String], rollback: &RollbackReport) -> ErrorCode {
    if !rollback.env_restored || !rollback.recreate_ok {
        ErrorCode::RollbackFailed
    } else {
        ErrorCode::ComponentUnavailable
    }
}

/// Preserve the disabled-start recovery guarantee: if dormant spam values are
/// invalid, disabling spam resets only that dormant scope to safe defaults so
/// the resident worker remains reachable.
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

            // Repair only fields that actually fail validation so an apply of
            // ENABLE_SPAM=false does not erase unrelated valid staged values.
            let mut repaired = merged.clone();
            let defaults = live_tuning::staged_map(&BTreeMap::new());
            let mut reset_keys = Vec::new();
            let spam = loop {
                match SpamTuning::from_source(&repaired) {
                    Ok((spam, _)) => break spam,
                    Err(error) => {
                        let keys: Vec<String> = config_error_details(&error)
                            .into_iter()
                            .filter_map(|detail| detail.key)
                            .filter(|key| {
                                live_tuning::spec(key).is_some_and(|spec| {
                                    spec.scope == ServiceScope::Spammer && key != "ENABLE_SPAM"
                                })
                            })
                            .collect();
                        let mut progressed = false;
                        for key in keys {
                            let default = defaults.get(&key).cloned().unwrap_or_default();
                            if repaired.get(&key) != Some(&default) {
                                repaired.insert(key.clone(), default);
                                reset_keys.push(key);
                                progressed = true;
                            }
                        }
                        // A cross-field constraint can still fail after its
                        // reported field reaches the default (for example a
                        // huge fill ratio with manual fanout). Reset the mode
                        // switch that makes that dormant constraint irrelevant.
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

/// Setting-aware post-apply verification: both workers stay resident and
/// report the exact effective policy, including spam's disabled phase.
fn verify(
    app: &AppState,
    staged: &LiveTuning,
    services: &[String],
    node1_reachable_before: bool,
) -> Result<(), String> {
    let names: Vec<&str> = services.iter().map(String::as_str).collect();
    for poll in 0..STABILIZE_POLLS {
        let last = poll == STABILIZE_POLLS - 1;
        let inspected = app
            .components
            .inspect_components(&names)
            .map_err(|error| format!("component inspection failed during verification: {error}"))?;

        for service in services {
            let scope = if service == ServiceScope::MiningController.service_name() {
                ServiceScope::MiningController
            } else {
                ServiceScope::Spammer
            };
            let Some(info) = inspected.get(service) else {
                return Err(format!("{service} not found after runtime apply"));
            };
            if info.restarting || info.restart_count > 0 {
                return Err(format!(
                    "{service} is restart-looping (status {}, exit code {})",
                    info.status, info.exit_code
                ));
            }
            // An acknowledged worker-policy mismatch will not heal within
            // this transaction, so fail immediately.
            if scope_needs_restart(staged, Some(&info.effective_config), scope) {
                return Err(format!(
                    "{service} started with an environment that does not match the applied settings"
                ));
            }
            if info.status == "exited" || info.status == "dead" {
                return Err(format!(
                    "{service} exited during the stabilization window (exit code {})",
                    info.exit_code
                ));
            }
            if last && !info.running {
                return Err(format!(
                    "{service} is not running at the end of the stabilization window (status {})",
                    info.status
                ));
            }
            if last
                && scope == ServiceScope::Spammer
                && !staged.spam.enabled
                && info.phase.as_deref() != Some("disabled")
            {
                return Err(format!(
                    "{service} did not reach the disabled phase (phase {})",
                    info.phase.as_deref().unwrap_or("unknown")
                ));
            }
        }
        if !last {
            app.job_actions.wait(POLL_INTERVAL);
        }
    }

    if node1_reachable_before && app.job_actions.node1_height().is_err() {
        return Err("node1 RPC became unreachable after the apply".to_string());
    }
    Ok(())
}

fn rollback(
    app: &AppState,
    original: &EnvFileState,
    written_revision: Option<&str>,
    services: &[String],
    runtime_before: &HashMap<String, ComponentInfo>,
    logs: &mut Vec<String>,
) -> RollbackReport {
    let mut messages: Vec<String> = Vec::new();

    let env_restored = if let Some(written_revision) = written_revision {
        let current = envfile::read_env_file(&original.path);
        let result = match current {
            Ok(current) if current.revision != written_revision => {
                messages.push(
                    "did not restore .env because it was edited after the panel write".to_string(),
                );
                None
            }
            Ok(_) if original.exists => {
                let ownership = original.ownership.unwrap_or(envfile::FileOwnership {
                    uid: 0,
                    gid: 0,
                    mode: 0o644,
                });
                Some(envfile::write_atomic(
                    &original.path,
                    &original.content,
                    ownership,
                ))
            }
            Ok(_) => Some(std::fs::remove_file(&original.path)),
            Err(error) => Some(Err(error)),
        };
        match result {
            Some(Ok(())) => {
                logs.push("rollback: restored previous .env state".to_string());
                true
            }
            Some(Err(error)) => {
                messages.push(format!("failed to restore .env: {error}"));
                false
            }
            None => false,
        }
    } else {
        true
    };

    let restore_services: Vec<String> = services
        .iter()
        .filter(|service| runtime_before.contains_key(*service))
        .cloned()
        .collect();
    let remove_services: Vec<String> = services
        .iter()
        .filter(|service| !runtime_before.contains_key(*service))
        .cloned()
        .collect();
    let mut restore_env = BTreeMap::new();
    let restore_generations: BTreeMap<String, u64> = runtime_before
        .iter()
        .filter_map(|(component, info)| {
            info.effective_generation
                .map(|generation| (component.clone(), generation))
        })
        .collect();
    for service in &restore_services {
        let scope = service_scope(service);
        let info = &runtime_before[service];
        for spec in live_tuning::MANAGED_SETTINGS
            .iter()
            .filter(|spec| spec.scope == scope)
        {
            if let Some(value) = info.effective_config.get(spec.key) {
                restore_env.insert(spec.key.to_string(), value.clone());
            }
        }
    }

    let restored = if restore_services.is_empty() {
        true
    } else {
        match app.configuration.restore_configuration(
            &restore_services,
            &restore_env,
            &restore_generations,
        ) {
            Ok(output) if output.success => {
                logs.push(format!(
                    "rollback: restored {} to the pre-apply runtime policy",
                    restore_services.join(", ")
                ));
                true
            }
            Ok(output) => {
                messages.push(format!(
                    "rollback restore exited non-zero: {}",
                    output.tail(5)
                ));
                false
            }
            Err(error) => {
                messages.push(format!("rollback restore failed: {error}"));
                false
            }
        }
    };
    let removed = if remove_services.is_empty() {
        true
    } else {
        match app.configuration.remove_components(&remove_services) {
            Ok(output) if output.success => {
                logs.push(format!(
                    "rollback: removed newly created {}",
                    remove_services.join(", ")
                ));
                true
            }
            Ok(output) => {
                messages.push(format!(
                    "rollback removal exited non-zero: {}",
                    output.tail(5)
                ));
                false
            }
            Err(error) => {
                messages.push(format!("rollback removal failed to launch: {error}"));
                false
            }
        }
    };
    let verified = restored
        && removed
        && verify_runtime_restored(app, services, runtime_before).map_or_else(
            |error| {
                messages.push(error);
                false
            },
            |()| true,
        );

    RollbackReport {
        env_restored,
        recreate_ok: verified,
        message: if messages.is_empty() {
            "rolled back to the previous configuration".to_string()
        } else {
            messages.join("; ")
        },
    }
}

fn service_scope(service: &str) -> ServiceScope {
    if service == ServiceScope::MiningController.service_name() {
        ServiceScope::MiningController
    } else {
        ServiceScope::Spammer
    }
}

fn verify_runtime_restored(
    app: &AppState,
    services: &[String],
    runtime_before: &HashMap<String, ComponentInfo>,
) -> Result<(), String> {
    let names: Vec<&str> = services.iter().map(String::as_str).collect();
    for poll in 0..STABILIZE_POLLS {
        let last = poll == STABILIZE_POLLS - 1;
        let inspected = app
            .components
            .inspect_components(&names)
            .map_err(|error| format!("rollback inspect failed: {error}"))?;
        for service in services {
            let Some(original) = runtime_before.get(service) else {
                if inspected.contains_key(service) {
                    return Err(format!("rollback left originally absent {service} present"));
                }
                continue;
            };
            let Some(restored) = inspected.get(service) else {
                return Err(format!("rollback did not restore {service}"));
            };
            for spec in live_tuning::MANAGED_SETTINGS
                .iter()
                .filter(|spec| spec.scope == service_scope(service))
            {
                if restored.effective_config.get(spec.key)
                    != original.effective_config.get(spec.key)
                {
                    return Err(format!(
                        "rollback restored the wrong {service} value for {}",
                        spec.key
                    ));
                }
            }
            if restored.restarting {
                return Err(format!("rollback left {service} restarting"));
            }
            if last && original.running && !restored.running {
                return Err(format!("rollback did not return {service} to running"));
            }
            if last
                && !original.running
                && (restored.status != original.status || restored.exit_code != original.exit_code)
            {
                return Err(format!(
                    "rollback did not restore {service} state (expected {} exit {}, got {} exit {})",
                    original.status, original.exit_code, restored.status, restored.exit_code
                ));
            }
        }
        if !last {
            app.job_actions.wait(POLL_INTERVAL);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MiningControlBackend, SpamControlBackend};
    use crate::state::{AppState, CONTROLLER_CONTAINER, SPAMMER_CONTAINER};
    use crate::test_support::{test_app, MockBackend, RecreateOutcome};
    use std::sync::Arc;

    struct Fixture {
        _dir: tempfile::TempDir,
        app: AppState,
        mock: Arc<MockBackend>,
    }

    fn fixture(env_content: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        if let Some(content) = env_content {
            std::fs::write(&env_file, content).expect("seed env");
        }
        let mock = Arc::new(MockBackend::new(env_file));
        mock.sync_containers();
        let app = test_app(dir.path(), mock.clone());
        Fixture {
            _dir: dir,
            app,
            mock,
        }
    }

    fn request(pairs: &[(&str, &str)]) -> ApplyRequest {
        ApplyRequest {
            settings: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            base_revision: None,
            base_generation: None,
        }
    }

    #[test]
    fn mining_only_change_applies_without_recreating_the_controller() {
        let fx = fixture(None);
        let report =
            apply(&fx.app, request(&[("BLOCK_INTERVAL_MEAN_SECS", "12")])).expect("apply succeeds");
        assert!(report.changed);
        assert!(report.file_changed);
        assert_eq!(report.components_applied, vec![CONTROLLER_CONTAINER]);
        assert!(report.legacy_services_recreated.is_empty());
        assert!(
            fx.mock.compose_calls().is_empty(),
            "mining policy must use the worker API"
        );
        let written = std::fs::read_to_string(&fx.app.config.env_file).expect("env written");
        assert!(written.contains("BLOCK_INTERVAL_MEAN_SECS=12"));
    }

    #[test]
    fn mining_apply_fails_before_writing_when_worker_is_unavailable() {
        let fx = fixture(None);
        fx.mock
            .world
            .lock()
            .expect("lock")
            .containers
            .remove(CONTROLLER_CONTAINER);
        let error = apply(&fx.app, request(&[("BLOCK_INTERVAL_MEAN_SECS", "12")]))
            .expect_err("worker must be reachable");
        assert_eq!(error.code, ErrorCode::ComponentUnavailable);
        assert!(!fx.app.config.env_file.exists());
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn spam_only_change_uses_worker_api_without_compose() {
        let fx = fixture(None);
        let report =
            apply(&fx.app, request(&[("SPAM_FILL_BLOCK_RATIO", "0.5")])).expect("apply succeeds");
        assert_eq!(report.components_applied, vec![SPAMMER_CONTAINER]);
        assert!(report.legacy_services_recreated.is_empty());
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn mixed_change_applies_both_workers_without_compose() {
        let fx = fixture(None);
        let report = apply(
            &fx.app,
            request(&[("MINER_WEIGHTS", "70,30"), ("ENABLE_SPAM_REPLACES", "true")]),
        )
        .expect("apply succeeds");
        assert_eq!(
            report.components_applied,
            vec![CONTROLLER_CONTAINER, SPAMMER_CONTAINER]
        );
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn mixed_apply_failure_restores_the_previous_mining_generation() {
        let fx = fixture(None);
        fx.mock.world.lock().expect("lock").compose_fail_times = 1;
        let error = apply(
            &fx.app,
            request(&[
                ("BLOCK_INTERVAL_MEAN_SECS", "12"),
                ("ENABLE_SPAM_REPLACES", "true"),
            ]),
        )
        .expect_err("spammer failure rolls back the transaction");
        assert!(error.rollback.expect("rollback").recreate_ok);
        let mining = MiningControlBackend::status(&*fx.mock).expect("mining status");
        assert_eq!(mining.effective_generation, 0);
        assert_eq!(mining.policy.mean_secs, 15);
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn fallback_fee_maps_to_spammer_only() {
        let fx = fixture(None);
        let report = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0002")])).expect("apply");
        assert_eq!(report.components_applied, vec![SPAMMER_CONTAINER]);
        for call in fx.mock.compose_calls() {
            assert!(
                !call.iter().any(|s| s.contains("node")),
                "nodes must never be touched"
            );
        }
    }

    #[test]
    fn true_noop_touches_nothing() {
        let fx = fixture(None);
        // First apply canonicalizes the file and recreates.
        apply(&fx.app, request(&[("FALLBACK_FEE", "0.0002")])).expect("first apply");
        let content_before = std::fs::read_to_string(&fx.app.config.env_file).expect("read");
        let calls_before = fx.mock.compose_calls().len();
        // Same value again: no file change, no recreate.
        let report = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0002")])).expect("noop");
        assert!(!report.changed);
        assert!(!report.file_changed);
        assert!(report.components_applied.is_empty());
        assert_eq!(fx.mock.compose_calls().len(), calls_before);
        assert_eq!(
            std::fs::read_to_string(&fx.app.config.env_file).expect("read"),
            content_before
        );
    }

    #[test]
    fn file_only_change_writes_without_recreating() {
        // .env stages a value that already equals what the containers run
        // with: the file must still be fixed (finding 6), but no recreate.
        let fx = fixture(Some("BLOCK_INTERVAL_MEAN_SECS=15\n"));
        // Containers already run the defaults (mean 15), so no service diff.
        let report = apply(&fx.app, request(&[])).expect("apply");
        assert!(report.changed);
        assert!(report.file_changed, "file must be canonicalized");
        assert!(report.components_applied.is_empty());
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn runtime_only_drift_recreates_without_rewriting() {
        let fx = fixture(None);
        apply(&fx.app, request(&[("SPAM_FLOOR_POOL_TXS", "1000")])).expect("first apply");
        let content_before = std::fs::read_to_string(&fx.app.config.env_file).expect("read");
        // Someone recreated the spammer with an older env behind our back.
        fx.mock
            .set_container_env(SPAMMER_CONTAINER, "SPAM_FLOOR_POOL_TXS", "4000");
        let report = apply(&fx.app, request(&[])).expect("apply");
        assert!(report.changed);
        assert!(!report.file_changed);
        assert_eq!(report.components_applied, vec![SPAMMER_CONTAINER]);
        assert_eq!(
            std::fs::read_to_string(&fx.app.config.env_file).expect("read"),
            content_before
        );
    }

    #[test]
    fn stale_revision_is_rejected() {
        let fx = fixture(Some("FALLBACK_FEE=0.0002\n"));
        let mut req = request(&[("FALLBACK_FEE", "0.0003")]);
        req.base_revision = Some("not-the-current-revision".to_string());
        let error = apply(&fx.app, req).expect_err("must reject");
        assert_eq!(error.code, ErrorCode::StaleRevision);
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn unknown_key_is_rejected_before_any_side_effect() {
        let fx = fixture(None);
        let error = apply(&fx.app, request(&[("BTC_RPC_PASS", "x")])).expect_err("must reject");
        assert_eq!(error.code, ErrorCode::ValidationFailed);
        assert!(!fx.app.config.env_file.exists(), "env must not be created");
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn invalid_merged_config_is_rejected() {
        let fx = fixture(None);
        let error = apply(&fx.app, request(&[("MINER_WEIGHTS", "0,0")])).expect_err("must reject");
        assert_eq!(error.code, ErrorCode::ValidationFailed);
        assert!(error
            .details
            .iter()
            .any(|d| d.key.as_deref() == Some("MINER_WEIGHTS")));
        assert!(!fx.app.config.env_file.exists());
    }

    #[test]
    fn enable_spam_input_is_strict() {
        // The spammer treats anything but the literal "true" as disabled, so
        // the panel refuses ambiguous forms instead of surprising the user.
        let fx = fixture(None);
        let error = apply(&fx.app, request(&[("ENABLE_SPAM", "1")])).expect_err("must reject");
        assert_eq!(error.code, ErrorCode::ValidationFailed);
    }

    #[test]
    fn fee_below_relay_minimum_is_rejected() {
        let fx = fixture(None);
        let error =
            apply(&fx.app, request(&[("FALLBACK_FEE", "0.000001")])).expect_err("must reject");
        assert_eq!(error.code, ErrorCode::ValidationFailed);
        assert!(error
            .details
            .iter()
            .any(|d| d.key.as_deref() == Some("FALLBACK_FEE")));
    }

    #[test]
    fn partial_apply_preserves_other_overrides() {
        let fx = fixture(None);
        apply(&fx.app, request(&[("FALLBACK_FEE", "0.0002")])).expect("first");
        apply(&fx.app, request(&[("MINER_WEIGHTS", "70,30")])).expect("second");
        let written = std::fs::read_to_string(&fx.app.config.env_file).expect("read");
        assert!(
            written.contains("FALLBACK_FEE=0.0002"),
            "first change must survive"
        );
        assert!(written.contains("MINER_WEIGHTS=70,30"));
    }

    #[test]
    fn empty_value_resets_to_default() {
        let fx = fixture(Some("MINER_WEIGHTS=70,30\n"));
        let report = apply(&fx.app, request(&[("MINER_WEIGHTS", "")])).expect("apply");
        assert!(report.changed);
        let written = std::fs::read_to_string(&fx.app.config.env_file).expect("read");
        assert!(written.contains("MINER_WEIGHTS=\n"));
    }

    #[test]
    fn unmanaged_lines_survive_and_legacy_aliases_migrate() {
        let fx = fixture(Some(
            "# my note\nBTC_IMAGE=custom:1\nSPAM_TXS_PER_BLOCK=500\n",
        ));
        apply(&fx.app, request(&[("FALLBACK_FEE", "0.0002")])).expect("apply");
        let written = std::fs::read_to_string(&fx.app.config.env_file).expect("read");
        assert!(written.contains("# my note\n"));
        assert!(written.contains("BTC_IMAGE=custom:1\n"));
        assert!(!written.contains("SPAM_TXS_PER_BLOCK="));
        assert!(written.contains("SPAM_FIXED_TXS_PER_BLOCK=500\n"));
    }

    #[test]
    fn worker_policy_failure_rolls_back_the_file() {
        let original = "FALLBACK_FEE=0.0002\n";
        let fx = fixture(Some(original));
        fx.mock.world.lock().expect("lock").compose_fail_times = 1;
        let error = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0003")])).expect_err("must fail");
        assert_eq!(error.code, ErrorCode::ComponentUnavailable);
        let rollback = error.rollback.expect("rollback report");
        assert!(rollback.env_restored);
        assert!(rollback.recreate_ok);
        assert_eq!(
            std::fs::read_to_string(&fx.app.config.env_file).expect("read"),
            original
        );
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn rollback_removes_a_newly_created_env_file() {
        let fx = fixture(None);
        fx.mock.world.lock().expect("lock").compose_fail_times = 1;
        let error = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0003")])).expect_err("must fail");
        assert!(error.rollback.expect("rollback").env_restored);
        assert!(
            !fx.app.config.env_file.exists(),
            "a file created by the failed apply must be removed, not left empty"
        );
    }

    #[test]
    fn post_start_crash_triggers_rollback() {
        let fx = fixture(Some("FALLBACK_FEE=0.0002\n"));
        fx.mock
            .world
            .lock()
            .expect("lock")
            .after_recreate
            .insert(SPAMMER_CONTAINER.to_string(), RecreateOutcome::Crash);
        let error = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0003")])).expect_err("must fail");
        assert_eq!(error.code, ErrorCode::ComponentUnavailable);
        assert!(error.message.contains("verification failed"));
        assert_eq!(
            std::fs::read_to_string(&fx.app.config.env_file).expect("read"),
            "FALLBACK_FEE=0.0002\n"
        );
    }

    #[test]
    fn mining_apply_ignores_legacy_container_restart_state() {
        let fx = fixture(None);
        fx.mock.world.lock().expect("lock").after_recreate.insert(
            CONTROLLER_CONTAINER.to_string(),
            RecreateOutcome::RestartLoop,
        );
        let report = apply(&fx.app, request(&[("BLOCK_INTERVAL_MEAN_SECS", "12")])).expect("apply");
        assert_eq!(report.components_applied, vec![CONTROLLER_CONTAINER]);
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn disabling_spam_keeps_the_worker_resident() {
        let fx = fixture(None);
        let report = apply(&fx.app, request(&[("ENABLE_SPAM", "false")])).expect("apply");
        assert_eq!(report.components_applied, vec![SPAMMER_CONTAINER]);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("spam is disabled")));
        let status = SpamControlBackend::status(&*fx.mock).expect("spam status");
        assert_eq!(
            status.phase,
            simchain_common::internal_api::WorkerPhase::Disabled
        );
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn disabling_spam_recovers_from_invalid_dormant_spam_values() {
        let fx = fixture(Some(
            "SPAM_FILL_BLOCK_RATIO=not-a-number\nFALLBACK_FEE=0.0002\nSPAM_FLOOR_POOL_TXS=1234\n",
        ));
        let report = apply(&fx.app, request(&[("ENABLE_SPAM", "false")]))
            .expect("disabling must not parse settings the spammer will ignore");
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("invalid dormant spam settings")));
        let written = std::fs::read_to_string(&fx.app.config.env_file).expect("read");
        assert!(written.contains("ENABLE_SPAM=false"));
        assert!(written.contains("SPAM_FILL_BLOCK_RATIO=2"));
        assert!(written.contains("FALLBACK_FEE=0.0002"));
        assert!(written.contains("SPAM_FLOOR_POOL_TXS=1234"));
        assert!(!written.contains("not-a-number"));
    }

    #[test]
    fn rollback_restores_the_pre_apply_runtime_not_the_old_file_rendering() {
        let fx = fixture(Some("FALLBACK_FEE=0.0002\n"));
        fx.mock
            .set_container_env(SPAMMER_CONTAINER, "FALLBACK_FEE", "0.00015");
        fx.mock
            .world
            .lock()
            .expect("lock")
            .after_recreate
            .insert(SPAMMER_CONTAINER.to_string(), RecreateOutcome::Crash);

        let error = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0003")]))
            .expect_err("new runtime crashes");
        assert!(error.rollback.expect("rollback").recreate_ok);
        let world = fx.mock.world.lock().expect("lock");
        assert_eq!(
            world.containers[SPAMMER_CONTAINER].effective_config["FALLBACK_FEE"],
            "0.00015"
        );
    }

    #[test]
    fn absent_spam_worker_fails_before_any_mutation() {
        let fx = fixture(None);
        fx.mock
            .world
            .lock()
            .expect("lock")
            .containers
            .remove(SPAMMER_CONTAINER);
        let error = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0003")]))
            .expect_err("worker is unavailable");
        assert_eq!(error.code, ErrorCode::ComponentUnavailable);
        assert!(error.rollback.is_none());
        assert!(!fx
            .mock
            .world
            .lock()
            .expect("lock")
            .containers
            .contains_key(SPAMMER_CONTAINER));
    }

    #[test]
    fn rollback_cas_preserves_a_newer_external_env_edit() {
        let fx = fixture(Some("FALLBACK_FEE=0.0002\n"));
        let original = envfile::read_env_file(&fx.app.config.env_file).expect("original");
        std::fs::write(&fx.app.config.env_file, "FALLBACK_FEE=0.0003\n").expect("panel write");
        let panel_revision = envfile::revision_of("FALLBACK_FEE=0.0003\n");
        std::fs::write(&fx.app.config.env_file, "# external edit\n").expect("external write");
        let mut logs = Vec::new();
        let report = rollback(
            &fx.app,
            &original,
            Some(&panel_revision),
            &[],
            &HashMap::new(),
            &mut logs,
        );
        assert!(!report.env_restored);
        assert!(report.recreate_ok);
        assert_eq!(
            std::fs::read_to_string(&fx.app.config.env_file).expect("read"),
            "# external edit\n"
        );
    }

    #[test]
    fn disabling_spam_rejects_a_crash_exit() {
        let fx = fixture(None);
        fx.mock
            .world
            .lock()
            .expect("lock")
            .after_recreate
            .insert(SPAMMER_CONTAINER.to_string(), RecreateOutcome::Crash);
        let error = apply(&fx.app, request(&[("ENABLE_SPAM", "false")])).expect_err("fail");
        assert!(error.message.contains("exit code 1"));
    }

    #[test]
    fn reenabling_spam_requires_running_again() {
        let fx = fixture(None);
        // Disable first; the worker remains resident.
        apply(&fx.app, request(&[("ENABLE_SPAM", "false")])).expect("disable");
        // Re-enable without process recreation.
        let report = apply(&fx.app, request(&[("ENABLE_SPAM", "true")])).expect("enable");
        assert_eq!(report.components_applied, vec![SPAMMER_CONTAINER]);
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[test]
    fn node1_down_before_apply_allows_non_fee_retunes() {
        // The post-apply liveness guard only applies when node1 was reachable
        // before the apply. Fee changes separately require live floor data.
        let fx = fixture(None);
        fx.mock.world.lock().expect("lock").node1_ok = false;
        let report = apply(&fx.app, request(&[("BLOCK_INTERVAL_MEAN_SECS", "12")]))
            .expect("non-fee retune may proceed when node1 was already down");
        assert!(report.changed);
    }

    #[test]
    fn fee_change_fails_closed_when_nodes_are_unreachable() {
        let fx = fixture(None);
        fx.mock.world.lock().expect("lock").node1_ok = false;
        let error = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0002")]))
            .expect_err("fee floor cannot be validated");
        assert_eq!(error.code, ErrorCode::RpcUnavailable);
        assert!(!fx.app.config.env_file.exists());
    }

    #[test]
    fn node1_loss_during_apply_triggers_rollback() {
        let fx = fixture(Some("FALLBACK_FEE=0.0002\n"));
        fx.mock.world.lock().expect("lock").kill_node1_on_recreate = true;
        let error = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0003")])).expect_err("must fail");
        assert!(error.message.contains("node1 RPC became unreachable"));
        assert_eq!(
            std::fs::read_to_string(&fx.app.config.env_file).expect("read"),
            "FALLBACK_FEE=0.0002\n"
        );
    }

    #[test]
    fn concurrent_applies_are_serialized_by_the_lock() {
        let fx = fixture(None);
        let _held = fx.app.apply_lock.lock().expect("hold lock");
        let error = apply(&fx.app, request(&[("FALLBACK_FEE", "0.0002")])).expect_err("busy");
        assert_eq!(error.code, ErrorCode::ApplyInProgress);
    }

    #[test]
    fn independent_file_lock_holder_blocks_apply() {
        let fx = fixture(None);
        let lock_path = fx.app.config.env_file.with_file_name(".env.panel.lock");
        let held = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)
            .expect("open lock");
        held.try_lock_exclusive().expect("hold external lock");
        let error = apply(&fx.app, request(&[("BLOCK_INTERVAL_MEAN_SECS", "12")]))
            .expect_err("external holder must win");
        assert_eq!(error.code, ErrorCode::ApplyInProgress);
    }
}
