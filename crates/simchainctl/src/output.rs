use crate::client::ClientError;
use simchain_common::control_api::{
    AbortJobResponse, ApplyReport, ComponentControlResponse, ConfigResponse, FaucetStatusResponse,
    FaucetTransfer, JobCheckpointResponse, JobCreatedResponse, JobDetail, JobEvent,
    JobListResponse, StatusResponse,
};
use std::io::{self, Write};

pub fn print_status(status: &StatusResponse, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json(status);
    }
    let mut out = io::stdout().lock();
    writeln!(out, "height: {}", optional(status.height))?;
    writeln!(
        out,
        "best hash: {}",
        status.best_hash.as_deref().unwrap_or("unavailable")
    )?;
    if let Some(mempool) = &status.mempool {
        writeln!(
            out,
            "mempool: {} txs, {} vB, {:.1} sat/vB minimum",
            mempool.tx_count,
            mempool.vbytes,
            mempool.min_fee * 100_000.0
        )?;
    }
    for (name, component) in &status.components {
        let reachability = if component.reachable {
            ""
        } else {
            " (unreachable)"
        };
        writeln!(out, "{name}: {}{reachability}", component.status)?;
    }
    if let Some(explorer) = &status.explorer {
        writeln!(
            out,
            "explorer: {} ({})",
            explorer.url,
            if explorer.reachable {
                "reachable"
            } else {
                "unreachable"
            }
        )?;
    }
    if let Some(error) = status.last_error.as_deref() {
        writeln!(out, "warning: {error}")?;
    }
    Ok(())
}

pub fn print_apply_report(report: &ApplyReport, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json(report);
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "generation {}: {}",
        report.generation,
        if report.changed {
            "applied"
        } else {
            "unchanged"
        }
    )?;
    if !report.components_applied.is_empty() {
        writeln!(out, "components: {}", report.components_applied.join(", "))?;
    }
    for warning in &report.warnings {
        writeln!(out, "warning: {warning}")?;
    }
    Ok(())
}

pub fn print_config(config: &ConfigResponse, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json(config);
    }
    let mut out = io::stdout().lock();
    writeln!(out, "generation: {}", config.generation)?;
    for (key, value) in &config.desired {
        writeln!(out, "{key}={value}")?;
    }
    if !config.pending_apply.is_empty() {
        writeln!(out, "pending apply: {}", config.pending_apply.join(", "))?;
    }
    Ok(())
}

pub fn print_component_control(response: &ComponentControlResponse) -> Result<(), ClientError> {
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{}: desired={}, effective={}, phase={}",
        response.component,
        response.desired_state.as_str(),
        response.effective_state.as_str(),
        response.phase.as_str(),
    )?;
    Ok(())
}

pub fn print_job_created(response: &JobCreatedResponse, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json(response);
    }
    let reused = if response.reused { " (reused)" } else { "" };
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{}: {}{}",
        response.job_id,
        response.state.as_str(),
        reused
    )?;
    Ok(())
}

pub fn print_faucet_created(
    response: &JobCreatedResponse,
    idempotency_key: &str,
    json: bool,
) -> Result<(), ClientError> {
    if json {
        return print_json(&serde_json::json!({
            "job_id": response.job_id,
            "state": response.state,
            "reused": response.reused,
            "idempotency_key": idempotency_key
        }));
    }
    let mut out = io::stdout().lock();
    writeln!(out, "{}: {}", response.job_id, response.state.as_str())?;
    writeln!(out, "idempotency key: {idempotency_key}")?;
    Ok(())
}

pub fn print_faucet_status(response: &FaucetStatusResponse, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json(response);
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "faucet: {}",
        if response.available {
            "available"
        } else {
            "unavailable"
        }
    )?;
    writeln!(out, "maximum request: {} sats", response.max_request_sats)?;
    writeln!(out, "wallet reserve: {} sats", response.wallet_reserve_sats)?;
    for wallet in &response.wallets {
        writeln!(
            out,
            "{} ({}): {} eligible sats, {} available after reserve",
            wallet.source.as_str(),
            wallet.wallet_name,
            wallet.eligible_confirmed_sats,
            wallet.available_after_reserve_sats
        )?;
    }
    if let Some(pending) = &response.pending_transfer {
        writeln!(
            out,
            "pending: {} ({})",
            pending.txid,
            pending.delivery_state.as_str()
        )?;
    }
    if let Some(error) = response.last_probe_error.as_deref() {
        writeln!(out, "warning: {error}")?;
    }
    Ok(())
}

pub fn print_faucet_transfer(transfer: &FaucetTransfer, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json(transfer);
    }
    let mut out = io::stdout().lock();
    writeln!(out, "SYSTEM FAUCET · 0 SAT FEE · MINER-PRIORITIZED")?;
    writeln!(out, "txid: {}", transfer.txid)?;
    writeln!(out, "delivery: {}", transfer.delivery_state.as_str())?;
    writeln!(
        out,
        "source: {} ({})",
        transfer.source.as_str(),
        transfer.wallet_name
    )?;
    writeln!(out, "total: {} sats", transfer.total_sats)?;
    writeln!(out, "actual fee: {} sats", transfer.actual_fee_sats)?;
    writeln!(
        out,
        "virtual priority: {} sats",
        transfer.priority_delta_sats
    )?;
    if let Some(height) = transfer.confirmed_height {
        writeln!(out, "confirmed height: {height}")?;
    }
    writeln!(out, "explorer: {}", transfer.explorer_url)?;
    if let Some(error) = transfer.last_error.as_deref() {
        writeln!(out, "warning: {error}")?;
    }
    Ok(())
}

pub fn print_job_id(response: &JobCreatedResponse) -> Result<(), ClientError> {
    let mut out = io::stdout().lock();
    writeln!(out, "{}", response.job_id)?;
    Ok(())
}

pub fn print_checkpoint(response: &JobCheckpointResponse, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json(response);
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{} / {}: {} (generation {})",
        response.job_id,
        response.checkpoint.name,
        response.checkpoint.state.as_str(),
        response.checkpoint.generation
    )?;
    Ok(())
}

pub fn print_jobs(response: &JobListResponse, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json(response);
    }
    let mut out = io::stdout().lock();
    for job in &response.jobs {
        writeln!(
            out,
            "{}  {:<12} {:<18} {}",
            job.id,
            job.kind.as_str(),
            job.state.as_str(),
            job.phase
        )?;
    }
    Ok(())
}

pub fn print_job_event(event: &JobEvent, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json_line(event);
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{:>6}  {:<24} {}",
        event.sequence, event.phase, event.message
    )?;
    Ok(())
}

pub fn print_job(job: &JobDetail, json: bool) -> Result<(), ClientError> {
    if json {
        return print_json(job);
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{}: {} ({})",
        job.summary.id,
        job.summary.state.as_str(),
        job.summary.phase
    )?;
    if let Some(failure) = &job.failure {
        writeln!(out, "failure: {}: {}", failure.code, failure.message)?;
    }
    if !job.summary.cleanup.errors.is_empty() {
        writeln!(
            out,
            "cleanup: {:?}: {}",
            job.summary.cleanup.state,
            job.summary.cleanup.errors.join("; ")
        )?;
    }
    Ok(())
}

pub fn print_abort(response: &AbortJobResponse) -> Result<(), ClientError> {
    let mut out = io::stdout().lock();
    writeln!(out, "{}: {}", response.job_id, response.state.as_str())?;
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> Result<(), ClientError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|error| ClientError::Output(error.to_string()))?;
    let mut out = io::stdout().lock();
    writeln!(out, "{json}")?;
    Ok(())
}

fn print_json_line(value: &impl serde::Serialize) -> Result<(), ClientError> {
    let json =
        serde_json::to_string(value).map_err(|error| ClientError::Output(error.to_string()))?;
    let mut out = io::stdout().lock();
    writeln!(out, "{json}")?;
    Ok(())
}

fn optional(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Output(error.to_string())
    }
}
