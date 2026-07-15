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
    /// Pause/resume continuous spam or submit a bounded burst.
    Spam(SpamArgs),
    /// Mine a bounded number of blocks through a server-side action job.
    Mine(MineArgs),
    /// Start a bounded server-side chain reorganization job.
    Reorg(ReorgArgs),
    /// Submit, run, or coordinate durable server-side scenarios.
    Scenario(ScenarioArgs),
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

#[derive(Debug, Subcommand)]
pub enum SpamCommand {
    /// Pause after already-submitted spam reaches a consistent boundary.
    Pause,
    /// Resume spam unless disabled by policy or held by a job-owned lease.
    Resume,
    /// Submit a bounded wallet transaction burst.
    Burst(SpamBurstArgs),
}

#[derive(Debug, Args)]
pub struct MineArgs {
    /// Miner node: node2 or node3.
    #[arg(long, default_value = "node2")]
    pub node: String,
    /// Positive number of blocks to mine.
    #[arg(long)]
    pub blocks: u64,
    /// Wait for the server-side action to finish.
    #[arg(long)]
    pub wait: bool,
    #[arg(long, default_value_t = 900)]
    pub timeout: u64,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct SpamBurstArgs {
    /// Wallet node: node2 or node3.
    #[arg(long, default_value = "node2")]
    pub node: String,
    /// Positive number of transactions to submit.
    #[arg(long)]
    pub txs: u64,
    /// Outputs per transaction; zero selects sequential sendtoaddress.
    #[arg(long, default_value_t = 0)]
    pub outputs_per_tx: u64,
    /// Wait for the server-side action to finish.
    #[arg(long)]
    pub wait: bool,
    #[arg(long, default_value_t = 900)]
    pub timeout: u64,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub idempotency_key: Option<String>,
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

#[derive(Debug, Args)]
pub struct ScenarioArgs {
    #[command(subcommand)]
    pub command: ScenarioCommand,
}

#[derive(Debug, Subcommand)]
pub enum ScenarioCommand {
    /// Upload a scenario and return immediately with its job ID.
    Start(ScenarioStartArgs),
    /// Upload a scenario and wait for its terminal result.
    Run(ScenarioRunArgs),
    /// Wait until one named checkpoint is durably reached.
    Wait(ScenarioWaitArgs),
    /// Release a reached pausing checkpoint using its current generation.
    Release(ScenarioReleaseArgs),
}

#[derive(Debug, Args)]
pub struct ScenarioStartArgs {
    /// Scenario YAML file.
    pub file: PathBuf,
    /// Emit the stable job-created JSON response.
    #[arg(long, conflicts_with = "id_only")]
    pub json: bool,
    /// Print only the server-assigned job ID for shell capture.
    #[arg(long, conflicts_with = "json")]
    pub id_only: bool,
    /// Optional retry key; identical retries return the original job.
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct ScenarioRunArgs {
    /// Scenario YAML file.
    pub file: PathBuf,
    /// Write the terminal job plus complete event/checkpoint summary as JSON.
    #[arg(long)]
    pub result: Option<PathBuf>,
    /// Emit stable JSON event and terminal objects.
    #[arg(long)]
    pub json: bool,
    /// Maximum terminal wait in seconds.
    #[arg(long, default_value_t = 900)]
    pub timeout: u64,
    /// Optional retry key; identical retries return the original job.
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct ScenarioWaitArgs {
    pub job_id: String,
    /// Checkpoint name from the submitted scenario.
    #[arg(long)]
    pub checkpoint: String,
    /// Maximum wait in seconds.
    #[arg(long, default_value_t = 900)]
    pub timeout: u64,
    /// Emit the stable checkpoint response as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ScenarioReleaseArgs {
    pub job_id: String,
    pub checkpoint: String,
    /// Emit the stable checkpoint response as JSON.
    #[arg(long)]
    pub json: bool,
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

        let scenario = Cli::try_parse_from([
            "simchainctl",
            "scenario",
            "start",
            "scenarios/ci.yml",
            "--id-only",
        ])
        .expect("scenario start");
        assert!(matches!(
            scenario.command,
            Command::Scenario(ScenarioArgs {
                command: ScenarioCommand::Start(ScenarioStartArgs { id_only: true, .. })
            })
        ));

        let wait = Cli::try_parse_from([
            "simchainctl",
            "scenario",
            "wait",
            "job-1",
            "--checkpoint",
            "mempool_loaded",
            "--timeout",
            "60",
        ])
        .expect("scenario wait");
        assert!(matches!(
            wait.command,
            Command::Scenario(ScenarioArgs {
                command: ScenarioCommand::Wait(ScenarioWaitArgs { timeout: 60, .. })
            })
        ));

        let mine = Cli::try_parse_from([
            "simchainctl",
            "mine",
            "--node",
            "node3",
            "--blocks",
            "2",
            "--wait",
        ])
        .expect("mine command");
        assert!(matches!(
            mine.command,
            Command::Mine(MineArgs {
                blocks: 2,
                wait: true,
                ..
            })
        ));

        let burst = Cli::try_parse_from([
            "simchainctl",
            "spam",
            "burst",
            "--txs",
            "10",
            "--outputs-per-tx",
            "4",
        ])
        .expect("spam burst");
        assert!(matches!(
            burst.command,
            Command::Spam(SpamArgs {
                command: SpamCommand::Burst(SpamBurstArgs {
                    txs: 10,
                    outputs_per_tx: 4,
                    ..
                })
            })
        ));
    }
}
