//! Desired/effective worker reconciliation. Worker restarts retain boot
//! policy but reset generation, so durable intent is continuously reapplied.

use crate::state::{AppState, SharedState};
use simchain_common::live_tuning::{LiveTuning, MiningTuning, SpamTuning};
use std::time::Duration;

const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReconcileOutcome {
    pub mining_policy_applied: bool,
    pub mining_state_applied: bool,
    pub spam_policy_applied: bool,
    pub spam_state_applied: bool,
}

impl ReconcileOutcome {
    fn changed(self) -> bool {
        self.mining_policy_applied
            || self.mining_state_applied
            || self.spam_policy_applied
            || self.spam_state_applied
    }
}

pub fn spawn(app: SharedState) {
    tokio::task::spawn_blocking(move || reconcile_loop(app));
}

fn reconcile_loop(app: SharedState) {
    let mut previous_error: Option<String> = None;
    loop {
        match reconcile_once(&app) {
            Ok(outcome) => {
                if previous_error.take().is_some() {
                    tracing::info!("worker reconciliation recovered");
                }
                if outcome.changed() {
                    tracing::info!(?outcome, "reconciled durable worker intent");
                }
            }
            Err(error) => {
                let message = error.to_string();
                if previous_error.as_deref() != Some(message.as_str()) {
                    tracing::warn!("worker reconciliation pending: {message}");
                    previous_error = Some(message);
                }
            }
        }
        std::thread::sleep(RECONCILE_INTERVAL);
    }
}

pub fn reconcile_once(app: &AppState) -> anyhow::Result<ReconcileOutcome> {
    let Ok(_guard) = app.apply_lock.try_lock() else {
        return Ok(ReconcileOutcome::default());
    };
    if app.jobs.active_summary().is_some() {
        return Ok(ReconcileOutcome::default());
    }
    let desired = app
        .control_state
        .read()
        .expect("control state lock")
        .clone();
    let (desired_tuning, _) = LiveTuning::from_source(&desired.desired)?;
    let mut outcome = ReconcileOutcome::default();
    let mut errors = Vec::new();

    if let Err(error) = (|| -> anyhow::Result<()> {
        let status = app.mining.status()?;
        if status.effective_generation != desired.generation
            || status.policy != desired_tuning.mining
        {
            apply_mining_policy(
                app,
                desired.generation,
                desired_tuning.mining.clone(),
                status.effective_generation,
            )?;
            outcome.mining_policy_applied = true;
        }
        if status.desired_state != desired.mining_state {
            app.mining.set_state(desired.mining_state)?;
            outcome.mining_state_applied = true;
        }
        Ok(())
    })() {
        errors.push(format!("mining: {error}"));
    }

    if let Err(error) = (|| -> anyhow::Result<()> {
        let status = app.spam.status()?;
        if status.effective_generation != desired.generation || status.policy != desired_tuning.spam
        {
            apply_spam_policy(
                app,
                desired.generation,
                desired_tuning.spam.clone(),
                status.effective_generation,
            )?;
            outcome.spam_policy_applied = true;
        }
        if status.desired_state != desired.spam_state {
            app.spam.set_state(desired.spam_state)?;
            outcome.spam_state_applied = true;
        }
        Ok(())
    })() {
        errors.push(format!("spam: {error}"));
    }

    if errors.is_empty() {
        Ok(outcome)
    } else {
        anyhow::bail!(errors.join("; "))
    }
}

fn apply_mining_policy(
    app: &AppState,
    generation: u64,
    policy: MiningTuning,
    effective_generation: u64,
) -> anyhow::Result<()> {
    if effective_generation < generation {
        app.mining.set_policy(generation, policy)?;
    } else {
        app.mining.restore_policy(generation, policy)?;
    }
    Ok(())
}

fn apply_spam_policy(
    app: &AppState,
    generation: u64,
    policy: SpamTuning,
    effective_generation: u64,
) -> anyhow::Result<()> {
    if effective_generation < generation {
        app.spam.set_policy(generation, policy)?;
    } else {
        app.spam.restore_policy(generation, policy)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CONTROLLER_CONTAINER, SPAMMER_CONTAINER};
    use crate::test_support::{test_app, MockBackend};
    use simchain_common::internal_api::DesiredState;
    use std::sync::Arc;

    #[test]
    fn reapplies_both_policies_and_manual_states_without_compose() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new(dir.path().join(".env")));
        mock.sync_containers();
        let app = test_app(dir.path(), mock.clone());

        {
            let mut state = app.control_state.write().expect("control state");
            state.generation = 4;
            state
                .desired
                .insert("BLOCK_INTERVAL_MEAN_SECS".to_string(), "12".to_string());
            state
                .desired
                .insert("SPAM_FILL_BLOCK_RATIO".to_string(), "3".to_string());
            state.mining_state = DesiredState::Paused;
            state.spam_state = DesiredState::Paused;
        }
        mock.set_container_env(CONTROLLER_CONTAINER, "BLOCK_INTERVAL_MEAN_SECS", "15");
        mock.set_container_env(SPAMMER_CONTAINER, "SPAM_FILL_BLOCK_RATIO", "2");

        let outcome = reconcile_once(&app).expect("reconcile");
        assert_eq!(
            outcome,
            ReconcileOutcome {
                mining_policy_applied: true,
                mining_state_applied: true,
                spam_policy_applied: true,
                spam_state_applied: true,
            }
        );
        assert_eq!(app.mining.status().expect("mining").effective_generation, 4);
        assert_eq!(app.spam.status().expect("spam").effective_generation, 4);
        assert_eq!(
            app.spam.status().expect("spam").desired_state,
            DesiredState::Paused
        );
        assert!(mock.compose_calls().is_empty());
    }

    #[test]
    fn matching_workers_are_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new(dir.path().join(".env")));
        mock.sync_containers();
        let app = test_app(dir.path(), mock);
        assert_eq!(
            reconcile_once(&app).expect("reconcile"),
            ReconcileOutcome::default()
        );
    }

    #[test]
    fn one_unavailable_worker_does_not_block_the_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new(dir.path().join(".env")));
        mock.sync_containers();
        let app = test_app(dir.path(), mock.clone());
        {
            let mut state = app.control_state.write().expect("control state");
            state.generation = 2;
            state
                .desired
                .insert("SPAM_FILL_BLOCK_RATIO".to_string(), "3".to_string());
        }
        mock.world
            .lock()
            .expect("world")
            .containers
            .remove(CONTROLLER_CONTAINER);

        let error = reconcile_once(&app).expect_err("mining stays unavailable");
        assert!(error.to_string().contains("mining"));
        let spam = app.spam.status().expect("spam still reconciled");
        assert_eq!(spam.effective_generation, 2);
        assert_eq!(spam.policy.fill_block_ratio, 3.0);
        assert!(mock.compose_calls().is_empty());
    }
}
