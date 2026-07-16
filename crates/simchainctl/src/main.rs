mod client;
mod commands;
mod output;

use clap::Parser;
use client::{ClientError, ControlClient};
use commands::{
    Cli, Command, ConfigCommand, DegradeCommand, FaucetCommand, JobsCommand, MiningCommand,
    PartitionCommand, ReorgCommand, ScenarioCommand, SpamCommand,
};
use simchain_common::control_api::{
    CheckpointState, CleanupState, ConfigPatchRequest, DegradeJobRequest, FaucetDeliveryState,
    FaucetJobRequest, FaucetOutput, FaucetSource, JobCheckpointResponse, JobDetail,
    JobEventsResponse, JobState, MineJobRequest, PartitionJobRequest, ReorgJobRequest,
    SpamBurstJobRequest,
};
use simchain_common::internal_api::DesiredState;
use std::collections::BTreeMap;
use std::process::ExitCode;

pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_OPERATION_FAILED: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_UNAVAILABLE: u8 = 3;
pub const EXIT_TIMEOUT: u8 = 4;
pub const EXIT_INTERRUPTED_OR_CLEANUP: u8 = 5;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::from(EXIT_SUCCESS),
        Err(error) => {
            eprintln!("simchainctl: {error}");
            ExitCode::from(exit_code(&error))
        }
    }
}

fn run(cli: Cli) -> Result<(), ClientError> {
    let connection = cli.connection.resolve();
    let client = ControlClient::new(connection.url, connection.token);
    match cli.command {
        Command::Status(args) => {
            if args.watch && args.interval_secs == 0 {
                return Err(ClientError::Local(
                    "--interval-secs must be positive".to_string(),
                ));
            }
            loop {
                output::print_status(&client.status()?, args.json)?;
                if !args.watch {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(args.interval_secs));
            }
        }
        Command::Config(config) => match config.command {
            ConfigCommand::Show(args) => {
                let config = client.config()?;
                output::print_config(&config, args.json)?;
            }
            ConfigCommand::Set(args) => {
                let settings = parse_assignments(&args.assignments)?;
                let report = client.patch_config(&ConfigPatchRequest {
                    settings,
                    base_generation: args.base_generation,
                })?;
                output::print_apply_report(&report, args.json)?;
            }
        },
        Command::Mining(mining) => {
            let state = match mining.command {
                MiningCommand::Pause => DesiredState::Paused,
                MiningCommand::Resume => DesiredState::Running,
            };
            let response = client.set_mining_state(state)?;
            output::print_component_control(&response)?;
        }
        Command::Spam(spam) => match spam.command {
            command @ (SpamCommand::Pause | SpamCommand::Resume) => {
                let state = match command {
                    SpamCommand::Pause => DesiredState::Paused,
                    SpamCommand::Resume => DesiredState::Running,
                    SpamCommand::Burst(_) => unreachable!("handled by outer match"),
                };
                let response = client.set_spam_state(state)?;
                output::print_component_control(&response)?;
            }
            SpamCommand::Burst(args) => {
                if args.txs == 0 {
                    return Err(ClientError::Local("--txs must be positive".to_string()));
                }
                let node = scenario_node(&args.node)?;
                let response = client.start_spam_burst(
                    &SpamBurstJobRequest {
                        node: node.to_string(),
                        txs: args.txs,
                        outputs_per_tx: args.outputs_per_tx,
                    },
                    args.idempotency_key.as_deref(),
                )?;
                output::print_job_created(&response, args.json)?;
                if args.wait {
                    let job = watch_job(&client, &response.job_id, args.json, args.timeout)?;
                    terminal_result(&job)?;
                }
            }
        },
        Command::Mine(args) => {
            if args.blocks == 0 {
                return Err(ClientError::Local("--blocks must be positive".to_string()));
            }
            let node = scenario_node(&args.node)?;
            let response = client.start_mine(
                &MineJobRequest {
                    node: node.to_string(),
                    blocks: args.blocks,
                },
                args.idempotency_key.as_deref(),
            )?;
            output::print_job_created(&response, args.json)?;
            if args.wait {
                let job = watch_job(&client, &response.job_id, args.json, args.timeout)?;
                terminal_result(&job)?;
            }
        }
        Command::Faucet(args) => match args.command {
            Some(FaucetCommand::Status(status)) => {
                output::print_faucet_status(&client.faucet_status()?, status.json)?;
            }
            Some(FaucetCommand::Transfer(transfer)) => {
                if transfer.watch {
                    let response = watch_faucet_transfer(
                        &client,
                        &transfer.txid,
                        transfer.json,
                        transfer.timeout,
                    )?;
                    ensure_faucet_delivery_succeeded(&response)?;
                } else {
                    output::print_faucet_transfer(
                        &client.faucet_transfer(&transfer.txid)?,
                        transfer.json,
                    )?;
                }
            }
            None => {
                if args.outputs.is_empty() {
                    return Err(ClientError::Local(
                        "faucet funding requires at least one --to ADDRESS=AMOUNT".to_string(),
                    ));
                }
                let outputs = parse_faucet_outputs(&args.outputs)?;
                let source = parse_faucet_source(&args.source)?;
                let idempotency_key = args
                    .idempotency_key
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let response =
                    client.start_faucet(&FaucetJobRequest { source, outputs }, &idempotency_key)?;
                output::print_faucet_created(&response, &idempotency_key, args.json)?;
                if args.wait {
                    let job = match watch_job(&client, &response.job_id, args.json, args.timeout) {
                        Err(ClientError::Timeout(_)) => {
                            return Err(ClientError::Timeout(format!(
                                "timed out waiting for faucet job {} (idempotency key {})",
                                response.job_id, idempotency_key
                            )))
                        }
                        other => other?,
                    };
                    terminal_result(&job)?;
                    if let Some(txid) = job
                        .result
                        .as_ref()
                        .and_then(|result| result.get("txid"))
                        .and_then(serde_json::Value::as_str)
                    {
                        output::print_faucet_transfer(&client.faucet_transfer(txid)?, args.json)?;
                    }
                }
            }
        },
        Command::Reorg(args) => {
            let args = match args.command {
                Some(ReorgCommand::Start(start)) => start,
                None => args.start,
            };
            let response = client.start_reorg(
                &ReorgJobRequest {
                    depth: args.depth,
                    empty: args.empty,
                    node: args.node,
                    adds_new_txs: args.adds_new_txs,
                    double_spend_pct: args.double_spend_pct,
                },
                args.idempotency_key.as_deref(),
            )?;
            output::print_job_created(&response, args.json)?;
            if args.wait {
                let job = watch_job(&client, &response.job_id, args.json, args.timeout)?;
                terminal_result(&job)?;
            }
        }
        Command::Partition(args) => {
            let args = match args.command {
                Some(PartitionCommand::Start(start)) => start,
                None => args.start,
            };
            let node = scenario_node(&args.node)?;
            let response = client.start_partition(
                &PartitionJobRequest {
                    node: node.to_string(),
                    main_blocks: args.main_blocks,
                    isolated_blocks: args.isolated_blocks,
                },
                args.idempotency_key.as_deref(),
            )?;
            output::print_job_created(&response, args.json)?;
            if args.wait {
                let job = watch_job(&client, &response.job_id, args.json, args.timeout)?;
                terminal_result(&job)?;
            }
        }
        Command::Degrade(args) => {
            let DegradeCommand::Start(args) = args.command;
            if args.delay_ms == 0 && args.loss_pct == 0.0 {
                return Err(ClientError::Local(
                    "degrade must specify a positive --delay-ms or --loss-pct".to_string(),
                ));
            }
            let node = network_node(&args.node)?;
            let response = client.start_degrade(
                &DegradeJobRequest {
                    node: node.to_string(),
                    delay_ms: args.delay_ms,
                    loss_pct: args.loss_pct,
                    seconds: args.seconds,
                },
                args.idempotency_key.as_deref(),
            )?;
            output::print_job_created(&response, args.json)?;
            if args.wait {
                let job = watch_job(&client, &response.job_id, args.json, args.timeout)?;
                terminal_result(&job)?;
            }
        }
        Command::Scenario(args) => match args.command {
            ScenarioCommand::Start(args) => {
                let yaml = read_scenario(&args.file)?;
                let response = client.start_scenario(yaml, args.idempotency_key.as_deref())?;
                if args.id_only {
                    output::print_job_id(&response)?;
                } else {
                    output::print_job_created(&response, args.json)?;
                }
            }
            ScenarioCommand::Run(args) => {
                let yaml = read_scenario(&args.file)?;
                let response = client.start_scenario(yaml, args.idempotency_key.as_deref())?;
                output::print_job_created(&response, args.json)?;
                let job = watch_job(&client, &response.job_id, args.json, args.timeout)?;
                if let Some(path) = args.result {
                    write_result_artifact(&client, &job, &path)?;
                }
                terminal_result(&job)?;
            }
            ScenarioCommand::Wait(args) => {
                let checkpoint =
                    wait_checkpoint(&client, &args.job_id, &args.checkpoint, args.timeout)?;
                output::print_checkpoint(&checkpoint, args.json)?;
            }
            ScenarioCommand::Release(args) => {
                let checkpoint = client.checkpoint(&args.job_id, &args.checkpoint)?;
                if checkpoint.checkpoint.generation == 0 {
                    return Err(ClientError::Api(format!(
                        "checkpoint '{}' has not been reached",
                        args.checkpoint
                    )));
                }
                let released = client.release_checkpoint(
                    &args.job_id,
                    &args.checkpoint,
                    checkpoint.checkpoint.generation,
                )?;
                output::print_checkpoint(&released, args.json)?;
            }
        },
        Command::Jobs(args) => match args.command {
            JobsCommand::List(args) => output::print_jobs(&client.jobs()?, args.json)?,
            JobsCommand::Watch(args) => {
                let job = watch_job(&client, &args.job_id, args.json, args.timeout)?;
                terminal_result(&job)?;
            }
            JobsCommand::Abort(args) => output::print_abort(&client.abort_job(&args.job_id)?)?,
        },
    }
    Ok(())
}

fn parse_assignments(assignments: &[String]) -> Result<BTreeMap<String, String>, ClientError> {
    let mut settings = BTreeMap::new();
    for assignment in assignments {
        let Some((key, value)) = assignment.split_once('=') else {
            return Err(ClientError::Local(format!(
                "invalid setting '{assignment}': expected KEY=VALUE"
            )));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(ClientError::Local(
                "setting key must not be empty".to_string(),
            ));
        }
        if settings
            .insert(key.to_string(), value.to_string())
            .is_some()
        {
            return Err(ClientError::Local(format!(
                "setting '{key}' was provided more than once"
            )));
        }
    }
    Ok(settings)
}

fn parse_faucet_outputs(values: &[String]) -> Result<Vec<FaucetOutput>, ClientError> {
    values
        .iter()
        .map(|value| {
            let Some((address, amount)) = value.rsplit_once('=') else {
                return Err(ClientError::Local(format!(
                    "invalid faucet destination '{value}': expected ADDRESS=AMOUNT"
                )));
            };
            let address = address.trim();
            if address.is_empty() {
                return Err(ClientError::Local(
                    "faucet destination address must not be empty".to_string(),
                ));
            }
            let amount_sats = if let Some(btc) = amount.strip_suffix("btc") {
                simchain_common::parse_btc_sats(btc).map_err(|error| {
                    ClientError::Local(format!("invalid faucet BTC amount '{amount}': {error}"))
                })?
            } else if let Some(sats) = amount.strip_suffix("sat") {
                if sats.is_empty() || !sats.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(ClientError::Local(format!(
                        "invalid faucet satoshi amount '{amount}'"
                    )));
                }
                sats.parse::<u64>().map_err(|_| {
                    ClientError::Local(format!("faucet satoshi amount '{amount}' is out of range"))
                })?
            } else {
                return Err(ClientError::Local(format!(
                    "faucet amount '{amount}' must end in btc or sat"
                )));
            };
            Ok(FaucetOutput {
                address: address.to_string(),
                amount_sats,
            })
        })
        .collect()
}

fn parse_faucet_source(value: &str) -> Result<FaucetSource, ClientError> {
    match value {
        "auto" => Ok(FaucetSource::Auto),
        "node2" => Ok(FaucetSource::Node2),
        "node3" => Ok(FaucetSource::Node3),
        _ => Err(ClientError::Local(
            "--source must be auto, node2, or node3".to_string(),
        )),
    }
}

fn watch_faucet_transfer(
    client: &ControlClient,
    txid: &str,
    json: bool,
    timeout_secs: u64,
) -> Result<simchain_common::control_api::FaucetTransfer, ClientError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut previous = None;
    loop {
        let transfer = client.faucet_transfer(txid)?;
        if previous != Some(transfer.delivery_state) {
            output::print_faucet_transfer(&transfer, json)?;
            previous = Some(transfer.delivery_state);
        }
        if matches!(
            transfer.delivery_state,
            FaucetDeliveryState::Confirmed
                | FaucetDeliveryState::DeliveryFailed
                | FaucetDeliveryState::AbortedAfterSubmission
                | FaucetDeliveryState::OrphanedAfterConfirmation
        ) {
            return Ok(transfer);
        }
        if std::time::Instant::now() >= deadline {
            return Err(ClientError::Timeout(format!(
                "timed out waiting for faucet transfer {txid}"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn ensure_faucet_delivery_succeeded(
    transfer: &simchain_common::control_api::FaucetTransfer,
) -> Result<(), ClientError> {
    if transfer.delivery_state == FaucetDeliveryState::Confirmed {
        Ok(())
    } else {
        Err(ClientError::Api(format!(
            "faucet transfer {} ended {}",
            transfer.txid,
            transfer.delivery_state.as_str()
        )))
    }
}

fn read_scenario(path: &std::path::Path) -> Result<String, ClientError> {
    std::fs::read_to_string(path).map_err(|error| {
        ClientError::Local(format!(
            "cannot read scenario file {}: {error}",
            path.display()
        ))
    })
}

fn scenario_node(node: &str) -> Result<&'static str, ClientError> {
    match node {
        "node2" | "btc-simnet-node2" => Ok("node2"),
        "node3" | "btc-simnet-node3" => Ok("node3"),
        _ => Err(ClientError::Local(
            "--node must be node2 or node3".to_string(),
        )),
    }
}

fn network_node(node: &str) -> Result<&'static str, ClientError> {
    match node {
        "node1" | "btc-simnet-node1" => Ok("node1"),
        "node2" | "btc-simnet-node2" => Ok("node2"),
        "node3" | "btc-simnet-node3" => Ok("node3"),
        _ => Err(ClientError::Local(
            "--node must be node1, node2, or node3".to_string(),
        )),
    }
}

fn wait_checkpoint(
    client: &ControlClient,
    job_id: &str,
    checkpoint: &str,
    timeout_secs: u64,
) -> Result<JobCheckpointResponse, ClientError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let response = client.checkpoint(job_id, checkpoint)?;
        match response.checkpoint.state {
            CheckpointState::Reached | CheckpointState::Released => return Ok(response),
            CheckpointState::TimedOut => {
                return Err(ClientError::Api(format!(
                    "checkpoint '{checkpoint}' timed out"
                )))
            }
            CheckpointState::Pending => {}
        }
        let job = client.job(job_id)?;
        if job.summary.state.is_terminal() {
            terminal_result(&job)?;
            return Err(ClientError::Api(format!(
                "job {job_id} ended before checkpoint '{checkpoint}' was reached"
            )));
        }
        if std::time::Instant::now() >= deadline {
            return Err(ClientError::Timeout(format!(
                "timed out waiting for checkpoint '{checkpoint}' on job {job_id}"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

#[derive(serde::Serialize)]
struct ScenarioArtifact<'a> {
    job: &'a JobDetail,
    events: JobEventsResponse,
}

fn write_result_artifact(
    client: &ControlClient,
    job: &JobDetail,
    path: &std::path::Path,
) -> Result<(), ClientError> {
    let mut after = 0u64;
    let mut all_events = Vec::new();
    loop {
        let page = client.job_events(&job.summary.id, after, 500)?;
        let page_len = page.events.len();
        after = page.next_sequence.max(after);
        all_events.extend(page.events);
        if page_len < 500 {
            break;
        }
    }
    let events = JobEventsResponse {
        events: all_events,
        next_sequence: after,
    };
    let artifact = serde_json::to_string_pretty(&ScenarioArtifact { job, events })
        .map_err(|error| ClientError::Output(error.to_string()))?;
    std::fs::write(path, format!("{artifact}\n")).map_err(|error| {
        ClientError::Local(format!(
            "cannot write scenario result {}: {error}",
            path.display()
        ))
    })
}

fn watch_job(
    client: &ControlClient,
    job_id: &str,
    json: bool,
    timeout_secs: u64,
) -> Result<JobDetail, ClientError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut after = 0u64;
    loop {
        let events = client.job_events(job_id, after, 200)?;
        for event in &events.events {
            output::print_job_event(event, json)?;
        }
        after = events.next_sequence.max(after);
        let job = client.job(job_id)?;
        if job.summary.state.is_terminal() {
            output::print_job(&job, json)?;
            return Ok(job);
        }
        if std::time::Instant::now() >= deadline {
            return Err(ClientError::Timeout(format!(
                "timed out waiting for job {job_id}"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn terminal_result(job: &JobDetail) -> Result<(), ClientError> {
    if job.summary.cleanup.state == CleanupState::Failed {
        return Err(ClientError::Interrupted(format!(
            "job {} cleanup failed: {}",
            job.summary.id,
            job.summary.cleanup.errors.join("; ")
        )));
    }
    match job.summary.state {
        JobState::Succeeded => Ok(()),
        JobState::Aborted | JobState::Interrupted => Err(ClientError::Interrupted(format!(
            "job {} ended {}",
            job.summary.id,
            job.summary.state.as_str()
        ))),
        JobState::Failed => Err(ClientError::Api(
            job.failure
                .as_ref()
                .map(|failure| failure.message.clone())
                .unwrap_or_else(|| format!("job {} failed", job.summary.id)),
        )),
        other => Err(ClientError::Api(format!(
            "job {} is not terminal ({})",
            job.summary.id,
            other.as_str()
        ))),
    }
}

fn exit_code(error: &ClientError) -> u8 {
    match error {
        ClientError::Unavailable(_) | ClientError::Authentication(_) => EXIT_UNAVAILABLE,
        ClientError::Timeout(_) => EXIT_TIMEOUT,
        ClientError::Interrupted(_) => EXIT_INTERRUPTED_OR_CLEANUP,
        ClientError::Local(_) => EXIT_USAGE,
        ClientError::Api(_) | ClientError::Decode(_) | ClientError::Output(_) => {
            EXIT_OPERATION_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_exit_codes_are_pinned() {
        assert_eq!(EXIT_SUCCESS, 0);
        assert_eq!(EXIT_OPERATION_FAILED, 1);
        assert_eq!(EXIT_USAGE, 2);
        assert_eq!(EXIT_UNAVAILABLE, 3);
        assert_eq!(EXIT_TIMEOUT, 4);
        assert_eq!(EXIT_INTERRUPTED_OR_CLEANUP, 5);
    }

    #[test]
    fn error_categories_map_to_stable_codes() {
        assert_eq!(
            exit_code(&ClientError::Unavailable("offline".to_string())),
            EXIT_UNAVAILABLE
        );
        assert_eq!(
            exit_code(&ClientError::Api("failed".to_string())),
            EXIT_OPERATION_FAILED
        );
        assert_eq!(
            exit_code(&ClientError::Timeout("slow".to_string())),
            EXIT_TIMEOUT
        );
        assert_eq!(
            exit_code(&ClientError::Interrupted("aborted".to_string())),
            EXIT_INTERRUPTED_OR_CLEANUP
        );
        assert_eq!(
            exit_code(&ClientError::Local("missing file".to_string())),
            EXIT_USAGE
        );
    }

    #[test]
    fn config_assignments_preserve_empty_resets_and_reject_duplicates() {
        let values = parse_assignments(&[
            "BLOCK_INTERVAL_MEAN_SECS=12".to_string(),
            "MINING_RNG_SEED=".to_string(),
        ])
        .expect("assignments");
        assert_eq!(values["BLOCK_INTERVAL_MEAN_SECS"], "12");
        assert_eq!(values["MINING_RNG_SEED"], "");
        assert!(parse_assignments(&["NO_EQUALS".to_string()]).is_err());
        assert!(parse_assignments(&["A=1".to_string(), "A=2".to_string()]).is_err());
    }

    #[test]
    fn faucet_destinations_parse_exact_suffixed_amounts() {
        let outputs = parse_faucet_outputs(&[
            "bcrt1qfirst=1.00000001btc".to_string(),
            "bcrt1psecond=25000000sat".to_string(),
        ])
        .expect("faucet outputs");
        assert_eq!(outputs[0].amount_sats, 100_000_001);
        assert_eq!(outputs[1].amount_sats, 25_000_000);
        assert!(parse_faucet_outputs(&["bcrt1qfirst=1.000000001btc".to_string()]).is_err());
        assert!(parse_faucet_outputs(&["bcrt1qfirst=1".to_string()]).is_err());
        assert!(parse_faucet_outputs(&["bcrt1qfirst=1BTC".to_string()]).is_err());
    }
}
