use crate::{
    config::Config,
    docker::Docker,
    results::RunSummary,
    rpc::{self, RpcClients},
    schema::{MinerNode, Scenario, Step, BOOTSTRAP_HEIGHT},
    steps,
};
use anyhow::Error;
use bitcoincore_rpc::RpcApi;
use std::time::Instant;

#[derive(Default)]
struct EngineState {
    mining_paused: bool,
    resume_failed: bool,
    active_partition: Option<MinerNode>,
}

impl EngineState {
    #[cfg(test)]
    fn cleanup_actions(&self) -> Vec<CleanupAction> {
        let mut actions = Vec::new();
        if let Some(node) = self.active_partition {
            actions.push(CleanupAction::Heal(node));
        }
        if self.mining_paused && !self.resume_failed {
            actions.push(CleanupAction::ResumeMining);
        }
        actions
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum CleanupAction {
    Heal(MinerNode),
    ResumeMining,
}

pub struct RunFailure {
    pub summary: RunSummary,
    pub source: Error,
}

pub struct Engine {
    config: Config,
    scenario: Scenario,
    rpc: RpcClients,
    docker: Docker,
    state: EngineState,
    started: Instant,
    executed_steps: usize,
}

impl Engine {
    pub fn new(config: Config, scenario: Scenario) -> anyhow::Result<Self> {
        let rpc = RpcClients::new(&config)?;
        let docker = Docker::new(config.repo_root.clone());
        Ok(Self {
            config,
            scenario,
            rpc,
            docker,
            state: EngineState::default(),
            started: Instant::now(),
            executed_steps: 0,
        })
    }

    pub fn run(mut self) -> Result<RunSummary, Box<RunFailure>> {
        tracing::info!("Waiting for node1 RPC");
        let initial_height = match rpc::wait_for_rpc(self.rpc.node1(), self.config.timeout) {
            Ok(height) => height,
            Err(error) => return Err(self.fail(error)),
        };
        tracing::info!(height = initial_height, "Node1 RPC is ready");
        tracing::info!(target_height = BOOTSTRAP_HEIGHT, "Waiting for bootstrap");
        let (_, height) =
            match rpc::wait_for_height(self.rpc.node1(), BOOTSTRAP_HEIGHT, self.config.timeout) {
                Ok(heights) => heights,
                Err(error) => return Err(self.fail(error)),
            };
        tracing::info!(height, "Bootstrap complete");

        for index in 0..self.scenario.steps.len() {
            let step = self.scenario.steps[index].clone();
            let step_number = index + 1;
            let step_started = Instant::now();
            let parameters =
                serde_json::to_string(&step).unwrap_or_else(|_| step.kind().to_string());
            tracing::info!(
                step = step_number,
                total = self.scenario.steps.len(),
                step_type = step.kind(),
                parameters,
                "Starting scenario step"
            );
            self.before_step(&step);
            if let Err(error) = steps::execute(&step, &self.rpc, &self.docker, self.config.timeout)
            {
                self.after_failed_step(&step);
                let error = error.context(format!(
                    "step {step_number}/{} ({}) failed",
                    self.scenario.steps.len(),
                    step.kind()
                ));
                self.cleanup();
                return Err(self.fail(error));
            }
            self.after_successful_step(&step);
            self.executed_steps += 1;
            tracing::info!(
                step = step_number,
                step_type = step.kind(),
                duration_ms = step_started.elapsed().as_millis(),
                "Finished scenario step"
            );
        }

        Ok(self.summary(true, None))
    }

    fn before_step(&mut self, step: &Step) {
        if let Step::Partition { node, .. } = step {
            self.state.active_partition = Some(*node);
        }
    }

    fn after_successful_step(&mut self, step: &Step) {
        match step {
            Step::PauseMining => self.state.mining_paused = true,
            Step::ResumeMining => {
                self.state.mining_paused = false;
                self.state.resume_failed = false;
            }
            Step::Partition { .. } => self.state.active_partition = None,
            _ => {}
        }
    }

    fn after_failed_step(&mut self, step: &Step) {
        if matches!(step, Step::ResumeMining) {
            self.state.resume_failed = true;
        }
    }

    // Cleanup reverses ENGINE-owned state only: partitions this engine started
    // and pauses this engine issued. Steps that shell out (reorg, partition)
    // stop/restart the controller and spammer through those scripts' own EXIT
    // traps -- e.g. a reorg failure that leaves the controller stopped is
    // simulate-reorg.sh's cleanup responsibility and is deliberately not
    // tracked here.
    fn cleanup(&mut self) {
        if let Some(node) = self.state.active_partition.take() {
            tracing::warn!(%node, "Healing partition after scenario failure");
            if let Err(error) = self.docker.heal_partition(&node.to_string()) {
                tracing::error!(%error, "Failed to heal partition during cleanup");
            }
        }
        if self.state.mining_paused && !self.state.resume_failed {
            tracing::warn!("Resuming mining controller after scenario failure");
            if let Err(error) = self.docker.resume_mining() {
                tracing::error!(%error, "Failed to resume mining during cleanup");
            }
            self.state.mining_paused = false;
        }
    }

    fn fail(&self, source: Error) -> Box<RunFailure> {
        Box::new(RunFailure {
            summary: self.summary(false, Some(format!("{source:#}"))),
            source,
        })
    }

    fn summary(&self, success: bool, error: Option<String>) -> RunSummary {
        let final_height = self.rpc.node1().get_block_count().ok();
        let best_block_hash = self
            .rpc
            .node1()
            .get_best_block_hash()
            .ok()
            .map(|hash| hash.to_string());
        RunSummary {
            scenario_file: self.config.scenario_file.display().to_string(),
            success,
            executed_steps: self.executed_steps,
            total_steps: self.scenario.steps.len(),
            duration_ms: self.started.elapsed().as_millis(),
            final_height,
            best_block_hash,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_resumes_after_engine_pause() {
        let state = EngineState {
            mining_paused: true,
            ..EngineState::default()
        };
        assert_eq!(state.cleanup_actions(), [CleanupAction::ResumeMining]);
    }

    #[test]
    fn cleanup_heals_failed_partition_before_resuming() {
        let state = EngineState {
            mining_paused: true,
            active_partition: Some(MinerNode::Node3),
            ..EngineState::default()
        };
        assert_eq!(
            state.cleanup_actions(),
            [
                CleanupAction::Heal(MinerNode::Node3),
                CleanupAction::ResumeMining
            ]
        );
    }

    #[test]
    fn failed_resume_is_not_retried_during_cleanup() {
        let state = EngineState {
            mining_paused: true,
            resume_failed: true,
            active_partition: None,
        };
        assert!(state.cleanup_actions().is_empty());
    }
}
