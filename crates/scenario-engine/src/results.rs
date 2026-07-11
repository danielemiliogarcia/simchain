use anyhow::{Context, Result};
use serde::Serialize;
use std::{fs, path::Path};

#[derive(Clone, Debug, Serialize)]
pub struct RunSummary {
    pub scenario_file: String,
    pub success: bool,
    pub executed_steps: usize,
    pub total_steps: usize,
    pub duration_ms: u128,
    pub final_height: Option<u64>,
    pub best_block_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RunSummary {
    pub fn write_json(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create result directory {}", parent.display())
            })?;
        }
        let body = serde_json::to_vec_pretty(self)?;
        fs::write(path, body)
            .with_context(|| format!("failed to write scenario result {}", path.display()))
    }
}
