mod client;
mod commands;
mod output;

use clap::Parser;
use client::{ClientError, ControlClient};
use commands::{Cli, Command, ConfigCommand, JobsCommand, MiningCommand, SpamCommand};
use simchain_common::control_api::{CleanupState, JobDetail, JobState, ReorgJobRequest};
use simchain_common::internal_api::DesiredState;
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
            let status = client.status()?;
            output::print_status(&status, args.json)?;
        }
        Command::Config(config) => match config.command {
            ConfigCommand::Show(args) => {
                let config = client.config()?;
                output::print_config(&config, args.json)?;
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
        Command::Spam(spam) => {
            let state = match spam.command {
                SpamCommand::Pause => DesiredState::Paused,
                SpamCommand::Resume => DesiredState::Running,
            };
            let response = client.set_spam_state(state)?;
            output::print_component_control(&response)?;
        }
        Command::Reorg(args) => {
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
    }
}
