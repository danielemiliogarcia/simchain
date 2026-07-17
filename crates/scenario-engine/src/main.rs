//! Thin file-based client for the control plane's scenario job API.
//! Scenario validation and execution live in the control plane; this binary
//! only submits YAML and follows the durable job.

mod config;

use anyhow::{bail, Context};
use config::Config;
use serde::de::DeserializeOwned;
use simchain_common::control_api::{
    ApiErrorEnvelope, CleanupState, JobCreatedResponse, JobDetail, JobEventsResponse,
    ScenarioJobRequest, API_PREFIX,
};
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    simchain_common::init_tracing("simchain_scenario_engine=info,info");
    let config = Config::from_env()?;
    wait_until_ready(&config)?;
    let yaml = std::fs::read_to_string(&config.scenario_file).with_context(|| {
        format!(
            "failed to read scenario file {}",
            config.scenario_file.display()
        )
    })?;
    let yaml = simchain_scenario_engine::Scenario::resolve_env_addresses_yaml(&yaml)
        .with_context(|| format!("invalid scenario file {}", config.scenario_file.display()))?;
    let created: JobCreatedResponse = post_json(
        &config,
        &format!("{API_PREFIX}/jobs/scenario"),
        &ScenarioJobRequest { yaml },
    )?;
    tracing::info!(job_id = %created.job_id, "Scenario job accepted by the control plane");

    let deadline = Instant::now() + config.timeout;
    let mut after = 0;
    let detail = loop {
        let events: JobEventsResponse = get(
            &config,
            &format!(
                "{API_PREFIX}/jobs/{}/events?after={after}&limit=200",
                created.job_id
            ),
        )?;
        for event in events.events {
            tracing::info!(
                sequence = event.sequence,
                phase = %event.phase,
                "{}",
                event.message
            );
        }
        after = after.max(events.next_sequence);
        let detail: JobDetail = get(&config, &format!("{API_PREFIX}/jobs/{}", created.job_id))?;
        if detail.summary.state.is_terminal() {
            break detail;
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for scenario job {}", created.job_id);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    };

    if let Some(path) = config.result_file {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&detail)?)?;
        tracing::info!(result_file = %path.display(), "Wrote scenario job artifact");
    }
    if detail.summary.cleanup.state == CleanupState::Failed {
        bail!(
            "scenario cleanup failed: {}",
            detail.summary.cleanup.errors.join("; ")
        );
    }
    match detail.summary.state {
        simchain_common::control_api::JobState::Succeeded => Ok(()),
        _ => bail!(
            "scenario job {} ended {}{}",
            detail.summary.id,
            detail.summary.state.as_str(),
            detail
                .failure
                .as_ref()
                .map(|failure| format!(": {}", failure.message))
                .unwrap_or_default()
        ),
    }
}

fn wait_until_ready(config: &Config) -> anyhow::Result<()> {
    let deadline = Instant::now() + config.timeout;
    let url = format!("{}/health/live", config.control_url);
    loop {
        if minreq::get(&url)
            .with_timeout(2)
            .send()
            .is_ok_and(|response| (200..300).contains(&response.status_code))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for the control plane at {url}");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn get<T: DeserializeOwned>(config: &Config, path: &str) -> anyhow::Result<T> {
    request(config, minreq::get(format!("{}{path}", config.control_url)))
}

fn post_json<T: DeserializeOwned>(
    config: &Config,
    path: &str,
    body: &impl serde::Serialize,
) -> anyhow::Result<T> {
    let body = serde_json::to_string(body)?;
    request(
        config,
        minreq::post(format!("{}{path}", config.control_url))
            .with_header("Content-Type", "application/json")
            .with_body(body),
    )
}

fn request<T: DeserializeOwned>(config: &Config, request: minreq::Request) -> anyhow::Result<T> {
    let response = request
        .with_timeout(35)
        .with_header("Authorization", format!("Bearer {}", config.token))
        .send()
        .context("control-plane request failed")?;
    let body = response.as_str()?;
    if !(200..300).contains(&response.status_code) {
        let message = serde_json::from_str::<ApiErrorEnvelope>(body)
            .map(|envelope| envelope.error.message)
            .unwrap_or_else(|_| format!("HTTP {}: {body}", response.status_code));
        bail!(message);
    }
    serde_json::from_str(body).context("invalid control-plane response")
}
