//! Desired/effective mining reconciliation. Worker restarts retain their
//! boot policy but reset their generation, so the control plane continuously
//! reapplies durable intent without making worker availability a startup
//! dependency.

use crate::state::{AppState, SharedState};
use simchain_common::live_tuning::MiningTuning;
use std::time::Duration;

const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReconcileOutcome {
    pub policy_applied: bool,
    pub state_applied: bool,
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
                    tracing::info!("mining worker reconciliation recovered");
                }
                if outcome.policy_applied || outcome.state_applied {
                    tracing::info!(
                        policy_applied = outcome.policy_applied,
                        state_applied = outcome.state_applied,
                        "reconciled durable mining intent"
                    );
                }
            }
            Err(error) => {
                let message = error.to_string();
                if previous_error.as_deref() != Some(message.as_str()) {
                    tracing::warn!("mining worker reconciliation pending: {message}");
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
    let desired = app
        .control_state
        .read()
        .expect("control state lock")
        .clone();
    let desired_policy = MiningTuning::from_source(&desired.desired)?;
    let status = app.mining.status()?;
    let mut outcome = ReconcileOutcome::default();

    if status.effective_generation != desired.generation || status.policy != desired_policy {
        if status.effective_generation < desired.generation {
            app.mining.set_policy(desired.generation, desired_policy)?;
        } else {
            // Covers an interrupted transaction that reached the worker but
            // did not durably commit, as well as same-generation corruption.
            app.mining
                .restore_policy(desired.generation, desired_policy)?;
        }
        outcome.policy_applied = true;
    }

    if status.desired_state != desired.mining_state {
        app.mining.set_state(desired.mining_state)?;
        outcome.state_applied = true;
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MiningControlBackend;
    use crate::state::CONTROLLER_CONTAINER;
    use crate::test_support::{test_app, MockBackend};
    use simchain_common::internal_api::DesiredState;
    use std::sync::Arc;

    #[test]
    fn reapplies_policy_and_manual_state_without_compose() {
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
            state.mining_state = DesiredState::Paused;
        }
        mock.set_container_env(CONTROLLER_CONTAINER, "BLOCK_INTERVAL_MEAN_SECS", "15");

        let outcome = reconcile_once(&app).expect("reconcile");
        assert_eq!(
            outcome,
            ReconcileOutcome {
                policy_applied: true,
                state_applied: true,
            }
        );
        let worker = mock.status().expect("worker status");
        assert_eq!(worker.effective_generation, 4);
        assert_eq!(worker.policy.mean_secs, 12);
        assert_eq!(worker.desired_state, DesiredState::Paused);
        assert!(mock.compose_calls().is_empty());
    }

    #[test]
    fn matching_worker_is_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new(dir.path().join(".env")));
        mock.sync_containers();
        let app = test_app(dir.path(), mock);
        assert_eq!(
            reconcile_once(&app).expect("reconcile"),
            ReconcileOutcome::default()
        );
    }
}
