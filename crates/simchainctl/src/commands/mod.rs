use clap::{Args, Parser, Subcommand};
use simchain_common::control_api::DEFAULT_CONTROL_URL;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "simchainctl", about = "Simchain control-plane client")]
pub struct Cli {
    #[command(flatten)]
    pub connection: ConnectionArgs,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct ConnectionArgs {
    /// Control-plane base URL (overrides SIMCHAIN_CONTROL_URL).
    #[arg(long, global = true)]
    pub url: Option<String>,
    /// Bearer token (overrides SIMCHAIN_CONTROL_TOKEN).
    #[arg(long, global = true)]
    pub token: Option<String>,
}

pub struct ResolvedConnection {
    pub url: String,
    pub token: Option<String>,
}

impl ConnectionArgs {
    pub fn resolve(self) -> ResolvedConnection {
        let url = self
            .url
            .or_else(|| std::env::var("SIMCHAIN_CONTROL_URL").ok())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CONTROL_URL.to_string());
        let token = self
            .token
            .or_else(|| std::env::var("SIMCHAIN_CONTROL_TOKEN").ok())
            .filter(|value| !value.trim().is_empty())
            .or_else(read_local_token);
        ResolvedConnection { url, token }
    }
}

fn read_local_token() -> Option<String> {
    let path = std::env::var("SIMCHAIN_CONTROL_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".simchain-control"))
        .join("token");
    std::fs::read_to_string(path)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show aggregate chain and component status.
    Status(StatusArgs),
    /// Inspect runtime configuration.
    Config(ConfigArgs),
    /// Pause or resume continuous mining at a worker safe point.
    Mining(MiningArgs),
    /// Pause or resume spam at a cooperative worker safe point.
    Spam(SpamArgs),
    /// Start a bounded server-side chain reorganization job.
    Reorg(ReorgArgs),
    /// Inspect, watch, or abort server-side jobs.
    Jobs(JobsArgs),
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Emit the stable JSON response.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show desired and effective runtime configuration.
    Show(JsonArgs),
}

#[derive(Debug, Args)]
pub struct JsonArgs {
    /// Emit the stable JSON response.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct MiningArgs {
    #[command(subcommand)]
    pub command: MiningCommand,
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub enum MiningCommand {
    /// Pause after any in-flight generate and propagation check completes.
    Pause,
    /// Resume continuous mining unless a job-owned pause lease remains.
    Resume,
}

#[derive(Debug, Args)]
pub struct SpamArgs {
    #[command(subcommand)]
    pub command: SpamCommand,
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub enum SpamCommand {
    /// Pause after already-submitted spam reaches a consistent boundary.
    Pause,
    /// Resume spam unless disabled by policy or held by a job-owned lease.
    Resume,
}

#[derive(Debug, Args)]
pub struct ReorgArgs {
    /// Number of tip blocks to replace.
    #[arg(long, default_value_t = 3)]
    pub depth: u64,
    /// Mine empty replacement blocks and leave orphaned transactions pending.
    #[arg(long)]
    pub empty: bool,
    /// Node that builds the replacement chain.
    #[arg(long, default_value = "node3")]
    pub node: String,
    /// Fresh wallet transactions to add to a non-empty replacement.
    #[arg(long, default_value_t = 0)]
    pub adds_new_txs: u64,
    /// Percentage of eligible orphaned wallet transactions to conflict.
    #[arg(long, default_value_t = 0)]
    pub double_spend_pct: u8,
    /// Wait for the job to reach a terminal state.
    #[arg(long)]
    pub wait: bool,
    /// Maximum wait in seconds when --wait is set.
    #[arg(long, default_value_t = 900)]
    pub timeout: u64,
    /// Emit stable JSON instead of human-oriented output.
    #[arg(long)]
    pub json: bool,
    /// Optional retry key; the server returns the original matching job.
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct JobsArgs {
    #[command(subcommand)]
    pub command: JobsCommand,
}

#[derive(Debug, Subcommand)]
pub enum JobsCommand {
    /// List bounded recent job history.
    List(JsonArgs),
    /// Poll structured events until a job is terminal.
    Watch(JobWatchArgs),
    /// Request cooperative abort and owned-resource cleanup.
    Abort(JobIdArgs),
}

#[derive(Debug, Args)]
pub struct JobWatchArgs {
    pub job_id: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long, default_value_t = 900)]
    pub timeout: u64,
}

#[derive(Debug, Args)]
pub struct JobIdArgs {
    pub job_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_control_commands() {
        let status =
            Cli::try_parse_from(["simchainctl", "status", "--json"]).expect("status command");
        assert!(matches!(
            status.command,
            Command::Status(StatusArgs { json: true })
        ));

        let spam = Cli::try_parse_from(["simchainctl", "spam", "resume"]).expect("spam command");
        assert!(matches!(
            spam.command,
            Command::Spam(SpamArgs {
                command: SpamCommand::Resume
            })
        ));

        let config =
            Cli::try_parse_from(["simchainctl", "config", "show"]).expect("config command");
        assert!(matches!(
            config.command,
            Command::Config(ConfigArgs {
                command: ConfigCommand::Show(JsonArgs { json: false })
            })
        ));

        let mining =
            Cli::try_parse_from(["simchainctl", "mining", "pause"]).expect("mining command");
        assert!(matches!(
            mining.command,
            Command::Mining(MiningArgs {
                command: MiningCommand::Pause
            })
        ));

        let reorg =
            Cli::try_parse_from(["simchainctl", "reorg", "--depth", "4", "--empty", "--wait"])
                .expect("reorg command");
        assert!(matches!(
            reorg.command,
            Command::Reorg(ReorgArgs {
                depth: 4,
                empty: true,
                wait: true,
                ..
            })
        ));

        let jobs = Cli::try_parse_from(["simchainctl", "jobs", "watch", "job-1", "--json"])
            .expect("jobs command");
        assert!(matches!(
            jobs.command,
            Command::Jobs(JobsArgs {
                command: JobsCommand::Watch(JobWatchArgs { json: true, .. })
            })
        ));
    }
}
