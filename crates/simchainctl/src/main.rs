mod client;
mod commands;
mod output;

use clap::Parser;
use client::{ClientError, ControlClient};
use commands::{Cli, Command, ConfigCommand, MiningCommand};
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
    }
    Ok(())
}

fn exit_code(error: &ClientError) -> u8 {
    match error {
        ClientError::Unavailable(_) | ClientError::Authentication(_) => EXIT_UNAVAILABLE,
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
    }
}
