//! Worker-first migration adapter. Mining and spam use their private control
//! APIs; the legacy backend remains only for node probes and later-phase jobs.

use crate::backend::{
    BackendOutput, ComponentBackend, ComponentInfo, ConfigurationBackend, JobActions,
    MiningControlBackend, SpamControlBackend,
};
use crate::compose::ComposeBackend;
use crate::state::{CONTROLLER_CONTAINER, SPAMMER_CONTAINER};
use simchain_common::live_tuning::{MiningTuning, SpamTuning};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

pub struct HybridBackend {
    legacy: ComposeBackend,
    mining: Arc<dyn MiningControlBackend>,
    spam: Arc<dyn SpamControlBackend>,
}

impl HybridBackend {
    pub fn new(
        legacy: ComposeBackend,
        mining: Arc<dyn MiningControlBackend>,
        spam: Arc<dyn SpamControlBackend>,
    ) -> Self {
        Self {
            legacy,
            mining,
            spam,
        }
    }
}

impl ComponentBackend for HybridBackend {
    fn inspect_components(&self, names: &[&str]) -> anyhow::Result<HashMap<String, ComponentInfo>> {
        let legacy_names: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| !is_worker(name))
            .collect();
        let mut components = if legacy_names.is_empty() {
            HashMap::new()
        } else {
            self.legacy.inspect_components(&legacy_names)?
        };
        if names.contains(&CONTROLLER_CONTAINER) {
            let component = match self.mining.status() {
                Ok(status) => ComponentInfo {
                    present: true,
                    status: status.phase.as_str().to_string(),
                    running: true,
                    restarting: false,
                    exit_code: 0,
                    restart_count: 0,
                    effective_config: status
                        .policy
                        .canonical_values()
                        .into_iter()
                        .map(|(key, value)| (key.to_string(), value))
                        .collect(),
                    phase: Some(status.phase.as_str().to_string()),
                    effective_generation: Some(status.effective_generation),
                    uptime_secs: Some(status.uptime_secs),
                    last_error: status.last_error,
                    desired_state: Some(status.desired_state),
                    effective_state: Some(status.effective_state),
                    observed_height: status.height,
                    next_scheduled_attempt_ms: status.next_scheduled_attempt_ms,
                    last_mined_block: status.last_mined_block,
                    active_lease_count: Some(status.active_leases.len()),
                    cycle_phase: None,
                    accepted_transactions: None,
                    reconciliation_pending: None,
                },
                Err(error) => unreachable_component(error),
            };
            components.insert(CONTROLLER_CONTAINER.to_string(), component);
        }
        if names.contains(&SPAMMER_CONTAINER) {
            let component = match self.spam.status() {
                Ok(status) => ComponentInfo {
                    present: true,
                    status: status.phase.as_str().to_string(),
                    running: true,
                    restarting: false,
                    exit_code: 0,
                    restart_count: 0,
                    effective_config: status
                        .policy
                        .canonical_values()
                        .into_iter()
                        .map(|(key, value)| (key.to_string(), value))
                        .collect(),
                    phase: Some(status.phase.as_str().to_string()),
                    effective_generation: Some(status.effective_generation),
                    uptime_secs: Some(status.uptime_secs),
                    last_error: status.last_error,
                    desired_state: Some(status.desired_state),
                    effective_state: Some(status.effective_state),
                    observed_height: status.observed_height,
                    next_scheduled_attempt_ms: None,
                    last_mined_block: None,
                    active_lease_count: Some(status.active_leases.len()),
                    cycle_phase: status.cycle_phase,
                    accepted_transactions: Some(status.accepted_transactions),
                    reconciliation_pending: Some(status.reconciliation_pending),
                },
                Err(error) => unreachable_component(error),
            };
            components.insert(SPAMMER_CONTAINER.to_string(), component);
        }
        Ok(components)
    }
}

impl ConfigurationBackend for HybridBackend {
    fn apply_configuration(
        &self,
        components: &[String],
        desired: &BTreeMap<String, String>,
        generation: u64,
    ) -> anyhow::Result<BackendOutput> {
        let mut outputs = Vec::new();
        if components.iter().any(|name| name == CONTROLLER_CONTAINER) {
            let policy = MiningTuning::from_source(desired)?;
            let ack = self.mining.set_policy(generation, policy)?;
            outputs.push(worker_output("mining", &ack));
        }
        if components.iter().any(|name| name == SPAMMER_CONTAINER) {
            let (policy, _) = SpamTuning::from_source(desired)?;
            let ack = self.spam.set_policy(generation, policy)?;
            outputs.push(worker_output("spam", &ack));
        }
        let legacy_components = without_workers(components);
        if !legacy_components.is_empty() {
            outputs.push(self.legacy.apply_configuration(
                &legacy_components,
                desired,
                generation,
            )?);
        }
        Ok(combine(outputs))
    }

    fn restore_configuration(
        &self,
        components: &[String],
        managed_values: &BTreeMap<String, String>,
        generations: &BTreeMap<String, u64>,
    ) -> anyhow::Result<BackendOutput> {
        let mut outputs = Vec::new();
        if components.iter().any(|name| name == CONTROLLER_CONTAINER) {
            let policy = MiningTuning::from_source(managed_values)?;
            let generation = generations.get(CONTROLLER_CONTAINER).copied().unwrap_or(0);
            let ack = self.mining.restore_policy(generation, policy)?;
            outputs.push(worker_output("mining rollback", &ack));
        }
        if components.iter().any(|name| name == SPAMMER_CONTAINER) {
            let (policy, _) = SpamTuning::from_source(managed_values)?;
            let generation = generations.get(SPAMMER_CONTAINER).copied().unwrap_or(0);
            let ack = self.spam.restore_policy(generation, policy)?;
            outputs.push(worker_output("spam rollback", &ack));
        }
        let legacy_components = without_workers(components);
        if !legacy_components.is_empty() {
            outputs.push(self.legacy.restore_configuration(
                &legacy_components,
                managed_values,
                generations,
            )?);
        }
        Ok(combine(outputs))
    }

    fn remove_components(&self, names: &[String]) -> anyhow::Result<BackendOutput> {
        if names.iter().any(|name| is_worker(name)) {
            anyhow::bail!("resident workers cannot be removed by configuration rollback");
        }
        self.legacy.remove_components(names)
    }
}

impl JobActions for HybridBackend {
    fn node1_height(&self) -> anyhow::Result<u64> {
        self.legacy.node1_height()
    }

    fn spam_min_fee(&self) -> anyhow::Result<f64> {
        self.legacy.spam_min_fee()
    }

    fn wait(&self, duration: Duration) {
        self.legacy.wait(duration);
    }
}

fn is_worker(name: &str) -> bool {
    name == CONTROLLER_CONTAINER || name == SPAMMER_CONTAINER
}

fn without_workers(components: &[String]) -> Vec<String> {
    components
        .iter()
        .filter(|name| !is_worker(name))
        .cloned()
        .collect()
}

fn worker_output(
    component: &str,
    ack: &simchain_common::internal_api::CommandAck,
) -> BackendOutput {
    BackendOutput {
        success: true,
        stdout: format!(
            "{component} policy generation {} applied at {}",
            ack.effective_generation,
            ack.phase.as_str()
        ),
        stderr: String::new(),
    }
}

fn unreachable_component(error: anyhow::Error) -> ComponentInfo {
    ComponentInfo {
        present: false,
        status: "unreachable".to_string(),
        running: false,
        restarting: false,
        exit_code: 0,
        restart_count: 0,
        effective_config: HashMap::new(),
        phase: None,
        effective_generation: None,
        uptime_secs: None,
        last_error: Some(error.to_string()),
        desired_state: None,
        effective_state: None,
        observed_height: None,
        next_scheduled_attempt_ms: None,
        last_mined_block: None,
        active_lease_count: None,
        cycle_phase: None,
        accepted_transactions: None,
        reconciliation_pending: None,
    }
}

fn combine(outputs: Vec<BackendOutput>) -> BackendOutput {
    BackendOutput {
        success: outputs.iter().all(|output| output.success),
        stdout: outputs
            .iter()
            .map(|output| output.stdout.trim())
            .filter(|output| !output.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        stderr: outputs
            .iter()
            .map(|output| output.stderr.trim())
            .filter(|output| !output.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}
