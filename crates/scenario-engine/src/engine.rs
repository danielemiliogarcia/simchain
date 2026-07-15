use crate::{CheckpointStep, MinerNode, Scenario, ScenarioResult, ScenarioStepResult, Step};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::thread;
use std::time::{Duration, Instant};

pub trait ScenarioControl: Send + Sync {
    fn observe(&self, progress: ScenarioProgress);
    fn abort_requested(&self) -> bool;
}

pub trait ScenarioActions: Send + Sync {
    fn wait_height(&self, height: u64, control: &dyn ScenarioControl) -> anyhow::Result<Value>;
    fn set_mining_paused(&self, paused: bool) -> anyhow::Result<Value>;
    fn mine(&self, node: MinerNode, blocks: u64) -> anyhow::Result<Value>;
    fn run_reorg(
        &self,
        depth: u64,
        empty: bool,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value>;
    fn spam_burst(
        &self,
        node: MinerNode,
        txs: u64,
        outputs_per_tx: u64,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value>;
    fn run_partition(
        &self,
        node: MinerNode,
        main_blocks: u64,
        isolated_blocks: u64,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value>;
    fn reach_checkpoint(
        &self,
        checkpoint: &CheckpointStep,
        step_index: usize,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value>;
    fn live_summary(&self) -> anyhow::Result<Value>;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioProgressPhase {
    StepStarted,
    StepCompleted,
    StepFailed,
    AbortObserved,
}

impl ScenarioProgressPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StepStarted => "step_started",
            Self::StepCompleted => "step_completed",
            Self::StepFailed => "step_failed",
            Self::AbortObserved => "abort_observed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScenarioProgress {
    pub phase: ScenarioProgressPhase,
    pub step_index: usize,
    pub total_steps: usize,
    pub step_kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

pub fn run(
    scenario: &Scenario,
    actions: &dyn ScenarioActions,
    control: &dyn ScenarioControl,
) -> ScenarioResult {
    let started = Instant::now();
    let mut steps = Vec::new();
    let mut error = None;
    let mut aborted = false;

    for (zero_index, step) in scenario.steps.iter().enumerate() {
        let step_index = zero_index + 1;
        if control.abort_requested() {
            aborted = true;
            control.observe(progress(
                ScenarioProgressPhase::AbortObserved,
                step_index,
                scenario.steps.len(),
                step,
                "scenario abort observed at a step boundary".to_string(),
                None,
            ));
            break;
        }

        control.observe(progress(
            ScenarioProgressPhase::StepStarted,
            step_index,
            scenario.steps.len(),
            step,
            format!(
                "starting step {step_index}/{} ({})",
                scenario.steps.len(),
                step.kind()
            ),
            serde_json::to_value(step).ok(),
        ));
        let step_started = Instant::now();
        let outcome = execute_step(step, step_index, actions, control);
        let duration_ms = elapsed_ms(step_started);
        match outcome {
            Ok(data) => {
                steps.push(ScenarioStepResult {
                    index: step_index,
                    kind: step.kind().to_string(),
                    duration_ms,
                    success: true,
                    error: None,
                });
                control.observe(progress(
                    ScenarioProgressPhase::StepCompleted,
                    step_index,
                    scenario.steps.len(),
                    step,
                    format!(
                        "completed step {step_index}/{} ({})",
                        scenario.steps.len(),
                        step.kind()
                    ),
                    Some(data),
                ));
                if control.abort_requested() {
                    aborted = true;
                    control.observe(progress(
                        ScenarioProgressPhase::AbortObserved,
                        step_index,
                        scenario.steps.len(),
                        step,
                        "scenario abort observed after the current safe action completed"
                            .to_string(),
                        None,
                    ));
                    break;
                }
            }
            Err(source) => {
                let message = format!(
                    "step {step_index}/{} ({}) failed: {source:#}",
                    scenario.steps.len(),
                    step.kind()
                );
                steps.push(ScenarioStepResult {
                    index: step_index,
                    kind: step.kind().to_string(),
                    duration_ms,
                    success: false,
                    error: Some(message.clone()),
                });
                control.observe(progress(
                    ScenarioProgressPhase::StepFailed,
                    step_index,
                    scenario.steps.len(),
                    step,
                    message.clone(),
                    None,
                ));
                error = Some(message);
                break;
            }
        }
    }

    let final_summary = match actions.live_summary() {
        Ok(summary) => Some(summary),
        Err(source) => {
            if error.is_none() && !aborted {
                error = Some(format!(
                    "failed to collect final scenario summary: {source:#}"
                ));
            }
            None
        }
    };
    let success = error.is_none() && !aborted && steps.len() == scenario.steps.len();
    ScenarioResult {
        success,
        aborted,
        executed_steps: steps.iter().filter(|step| step.success).count(),
        total_steps: scenario.steps.len(),
        duration_ms: elapsed_ms(started),
        steps,
        final_summary,
        error,
    }
}

fn execute_step(
    step: &Step,
    step_index: usize,
    actions: &dyn ScenarioActions,
    control: &dyn ScenarioControl,
) -> anyhow::Result<Value> {
    match step {
        Step::WaitHeight { height } => actions.wait_height(*height, control),
        Step::Sleep { secs } => {
            let deadline = Instant::now() + Duration::from_secs(*secs);
            while Instant::now() < deadline {
                if control.abort_requested() {
                    return Ok(json!({"aborted_during_sleep": true}));
                }
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(100)),
                );
            }
            Ok(json!({"slept_secs": secs}))
        }
        Step::PauseMining => actions.set_mining_paused(true),
        Step::ResumeMining => actions.set_mining_paused(false),
        Step::Mine { node, blocks } => actions.mine(*node, *blocks),
        Step::Reorg { depth, empty } => actions.run_reorg(*depth, *empty, control),
        Step::SpamBurst {
            node,
            txs,
            outputs_per_tx,
        } => actions.spam_burst(*node, *txs, *outputs_per_tx, control),
        Step::Partition {
            node,
            main_blocks,
            isolated_blocks,
        } => actions.run_partition(*node, *main_blocks, *isolated_blocks, control),
        Step::Checkpoint { checkpoint } => {
            actions.reach_checkpoint(checkpoint, step_index, control)
        }
    }
}

fn progress(
    phase: ScenarioProgressPhase,
    step_index: usize,
    total_steps: usize,
    step: &Step,
    message: String,
    data: Option<Value>,
) -> ScenarioProgress {
    ScenarioProgress {
        phase,
        step_index,
        total_steps,
        step_kind: step.kind().to_string(),
        message,
        data,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        abort: AtomicBool,
    }

    impl ScenarioControl for Fake {
        fn observe(&self, progress: ScenarioProgress) {
            self.calls
                .lock()
                .unwrap()
                .push(progress.phase.as_str().to_string());
        }

        fn abort_requested(&self) -> bool {
            self.abort.load(Ordering::Acquire)
        }
    }

    impl ScenarioActions for Fake {
        fn wait_height(&self, height: u64, _: &dyn ScenarioControl) -> anyhow::Result<Value> {
            Ok(json!({"height": height}))
        }
        fn set_mining_paused(&self, paused: bool) -> anyhow::Result<Value> {
            Ok(json!({"paused": paused}))
        }
        fn mine(&self, _: MinerNode, blocks: u64) -> anyhow::Result<Value> {
            Ok(json!({"blocks": blocks}))
        }
        fn run_reorg(&self, depth: u64, _: bool, _: &dyn ScenarioControl) -> anyhow::Result<Value> {
            Ok(json!({"depth": depth}))
        }
        fn spam_burst(
            &self,
            _: MinerNode,
            txs: u64,
            _: u64,
            _: &dyn ScenarioControl,
        ) -> anyhow::Result<Value> {
            Ok(json!({"txs": txs}))
        }
        fn run_partition(
            &self,
            _: MinerNode,
            main_blocks: u64,
            isolated_blocks: u64,
            _: &dyn ScenarioControl,
        ) -> anyhow::Result<Value> {
            Ok(json!({"main": main_blocks, "isolated": isolated_blocks}))
        }
        fn reach_checkpoint(
            &self,
            checkpoint: &CheckpointStep,
            _: usize,
            _: &dyn ScenarioControl,
        ) -> anyhow::Result<Value> {
            Ok(json!({"checkpoint": checkpoint.name}))
        }
        fn live_summary(&self) -> anyhow::Result<Value> {
            Ok(json!({"height": 210}))
        }
    }

    #[test]
    fn runs_steps_in_order_and_reports_structured_progress() {
        let scenario = Scenario::parse(
            "version: 1\nsteps:\n  - type: pause_mining\n  - type: mine\n    node: btc-simnet-node2\n    blocks: 2\n",
        )
        .unwrap();
        let fake = Fake::default();
        let result = run(&scenario, &fake, &fake);
        assert!(result.success);
        assert_eq!(result.executed_steps, 2);
        assert_eq!(
            *fake.calls.lock().unwrap(),
            [
                "step_started",
                "step_completed",
                "step_started",
                "step_completed"
            ]
        );
    }

    #[test]
    fn abort_stops_at_the_next_step_boundary() {
        let scenario = Scenario::parse(
            "version: 1\nsteps:\n  - type: pause_mining\n  - type: resume_mining\n",
        )
        .unwrap();
        let fake = Fake::default();
        fake.abort.store(true, Ordering::Release);
        let result = run(&scenario, &fake, &fake);
        assert!(result.aborted);
        assert_eq!(result.executed_steps, 0);
    }
}
