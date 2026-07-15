//! Domain-facing backend ports. The control-plane service and tests depend
//! on these traits, not on Docker/Compose. Phase 1 keeps a legacy Compose
//! adapter behind them; later phases replace it component by component.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ComponentInfo {
    pub status: String,
    pub running: bool,
    pub restarting: bool,
    pub exit_code: i64,
    pub restart_count: i64,
    pub effective_config: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct BackendOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl BackendOutput {
    pub fn tail(&self, lines: usize) -> String {
        let combined = format!("{}\n{}", self.stdout.trim(), self.stderr.trim());
        let all: Vec<&str> = combined
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let start = all.len().saturating_sub(lines);
        all[start..].join("\n")
    }
}

/// Component reachability, lifecycle telemetry, and effective configuration.
pub trait ComponentBackend: Send + Sync {
    fn inspect_components(&self, names: &[&str]) -> anyhow::Result<HashMap<String, ComponentInfo>>;
}

/// Applies desired runtime configuration. Method names are deliberately
/// transport-neutral even though the Phase-1 adapter recreates containers.
pub trait ConfigurationBackend: Send + Sync {
    fn apply_configuration(&self, components: &[String]) -> anyhow::Result<BackendOutput>;

    fn restore_configuration(
        &self,
        components: &[String],
        managed_values: &BTreeMap<String, String>,
    ) -> anyhow::Result<BackendOutput>;

    fn remove_components(&self, names: &[String]) -> anyhow::Result<BackendOutput>;
}

/// Bounded action/probe port used by configuration and, in later phases, by
/// jobs. It contains no process-lifecycle or Docker vocabulary.
pub trait JobActions: Send + Sync {
    fn node1_height(&self) -> anyhow::Result<u64>;
    fn spam_min_fee(&self) -> anyhow::Result<f64>;
    fn wait(&self, duration: Duration);
}
