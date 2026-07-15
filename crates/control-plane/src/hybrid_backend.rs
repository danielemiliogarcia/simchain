//! Phase-2 migration adapter: mining uses its internal worker API while spam
//! and process telemetry still use the legacy Compose adapter.

use crate::backend::{
    BackendOutput, ComponentBackend, ComponentInfo, ConfigurationBackend, JobActions,
    MiningControlBackend,
};
use crate::compose::ComposeBackend;
use crate::state::CONTROLLER_CONTAINER;
use simchain_common::live_tuning::MiningTuning;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

pub struct HybridBackend {
    legacy: ComposeBackend,
    mining: Arc<dyn MiningControlBackend>,
}

impl HybridBackend {
    pub fn new(legacy: ComposeBackend, mining: Arc<dyn MiningControlBackend>) -> Self {
        Self { legacy, mining }
    }
}

impl ComponentBackend for HybridBackend {
    fn inspect_components(&self, names: &[&str]) -> anyhow::Result<HashMap<String, ComponentInfo>> {
        let legacy_names: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| *name != CONTROLLER_CONTAINER)
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
                },
                Err(error) => ComponentInfo {
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
                },
            };
            components.insert(CONTROLLER_CONTAINER.to_string(), component);
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
            outputs.push(BackendOutput {
                success: true,
                stdout: format!(
                    "mining policy generation {} applied at {}",
                    ack.effective_generation,
                    ack.phase.as_str()
                ),
                stderr: String::new(),
            });
        }
        let legacy_components = without_mining(components);
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
            outputs.push(BackendOutput {
                success: true,
                stdout: format!(
                    "restored mining policy generation {}",
                    ack.effective_generation
                ),
                stderr: String::new(),
            });
        }
        let legacy_components = without_mining(components);
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
        if names.iter().any(|name| name == CONTROLLER_CONTAINER) {
            anyhow::bail!(
                "the mining worker is unavailable; it cannot be removed by configuration rollback"
            );
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

fn without_mining(components: &[String]) -> Vec<String> {
    components
        .iter()
        .filter(|name| name.as_str() != CONTROLLER_CONTAINER)
        .cloned()
        .collect()
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
