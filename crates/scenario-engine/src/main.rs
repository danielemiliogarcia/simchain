mod burst;
mod config;
mod docker;
mod engine;
mod results;
mod rpc;
mod schema;
mod steps;

use anyhow::Result;
use config::Config;
use engine::Engine;
use schema::Scenario;
use simchain_common::{config::CommonConfig, init_tracing};

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing("simchain_scenario_engine=info,info");
    CommonConfig::init()?;
    let config = Config::from_env()?;
    tracing::info!(scenario_file = %config.scenario_file.display(), "Loading scenario");
    let scenario = Scenario::load(&config.scenario_file)?;
    tracing::info!(steps = scenario.steps.len(), "Scenario validated");
    let result_file = config.result_file.clone();

    match Engine::new(config, scenario)?.run() {
        Ok(summary) => {
            log_summary(&summary);
            if let Some(path) = result_file {
                summary.write_json(&path)?;
                tracing::info!(result_file = %path.display(), "Wrote scenario result");
            }
            Ok(())
        }
        Err(failure) => {
            log_summary(&failure.summary);
            if let Some(path) = result_file {
                if let Err(error) = failure.summary.write_json(&path) {
                    tracing::error!(%error, "Failed to write scenario result");
                }
            }
            Err(failure.source)
        }
    }
}

fn log_summary(summary: &results::RunSummary) {
    if summary.success {
        tracing::info!(
            executed_steps = summary.executed_steps,
            duration_ms = summary.duration_ms,
            final_height = ?summary.final_height,
            best_block_hash = ?summary.best_block_hash,
            "Scenario completed successfully"
        );
    } else {
        tracing::error!(
            executed_steps = summary.executed_steps,
            duration_ms = summary.duration_ms,
            final_height = ?summary.final_height,
            best_block_hash = ?summary.best_block_hash,
            error = ?summary.error,
            "Scenario failed"
        );
    }
}
