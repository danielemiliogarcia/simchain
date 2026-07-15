use crate::client::ClientError;
use simchain_common::control_api::{ConfigResponse, StatusResponse};
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
        writeln!(out, "{}: {}", short_component(name), component.status)?;
    }
    if let Some(error) = status.last_error.as_deref() {
        writeln!(out, "warning: {error}")?;
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

fn print_json(value: &impl serde::Serialize) -> Result<(), ClientError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|error| ClientError::Output(error.to_string()))?;
    let mut out = io::stdout().lock();
    writeln!(out, "{json}")?;
    Ok(())
}

fn optional(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}

fn short_component(name: &str) -> &str {
    name.strip_prefix("btc-simnet-").unwrap_or(name)
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Output(error.to_string())
    }
}
