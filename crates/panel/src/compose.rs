//! Process execution behind a trait so apply/verify logic is testable
//! without Docker, plus the real Docker/compose/RPC implementation.

use crate::docker_inspect::{parse_inspect_output, ContainerInfo};
use bitcoincore_rpc::RpcApi;
use simchain_common::live_tuning;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    /// A short tail of the combined output for API/MCP result logs.
    pub fn tail(&self, lines: usize) -> String {
        let combined = format!("{}\n{}", self.stdout.trim(), self.stderr.trim());
        let all: Vec<&str> = combined.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = all.len().saturating_sub(lines);
        all[start..].join("\n")
    }
}

/// Everything the apply transaction and the status sampler need from the
/// outside world. Mocked in tests.
pub trait Executor: Send + Sync {
    /// `docker compose ... up -d --no-deps --force-recreate <services>`.
    fn compose_recreate(&self, services: &[String]) -> anyhow::Result<CommandOutput>;
    /// Recreate with explicit managed values. Used only for rollback, where
    /// the pre-apply running environment—not the possibly dirty `.env`—is the
    /// state that must be restored.
    fn compose_recreate_with_env(
        &self,
        services: &[String],
        managed_env: &BTreeMap<String, String>,
    ) -> anyhow::Result<CommandOutput>;
    /// Remove containers that did not exist before a failed apply.
    fn remove_containers(&self, names: &[String]) -> anyhow::Result<CommandOutput>;
    /// `docker inspect` on pinned container names; missing names are absent.
    fn inspect(&self, names: &[&str]) -> anyhow::Result<HashMap<String, ContainerInfo>>;
    /// Cheap node1 RPC liveness probe; the current height on success.
    fn node1_height(&self) -> anyhow::Result<u64>;
    /// Highest currently enforced relay/mempool minimum across the three
    /// nodes, in BTC/kvB.
    fn spam_min_fee(&self) -> anyhow::Result<f64>;
    /// Stabilization-window sleep, instant in tests.
    fn sleep(&self, duration: Duration);
}

pub struct SystemExecutor {
    repo_root: PathBuf,
    env_file: PathBuf,
    project: String,
    node_urls: Vec<String>,
}

impl SystemExecutor {
    pub fn new(
        repo_root: PathBuf,
        env_file: PathBuf,
        project: String,
        node_urls: Vec<String>,
    ) -> Self {
        Self {
            repo_root,
            env_file,
            project,
            node_urls,
        }
    }

    fn run_raw(&self, mut command: Command) -> anyhow::Result<CommandOutput> {
        let output = command.output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn run(&self, mut command: Command) -> anyhow::Result<CommandOutput> {
        scrub_managed_env(&mut command);
        self.run_raw(command)
    }
}

impl SystemExecutor {
    /// The full compose invocation, exposed for tests.
    fn compose_command(&self, services: &[String]) -> Command {
        let mut command = Command::new("docker");
        command
            .arg("compose")
            .arg("-f")
            .arg(self.repo_root.join("docker-compose.yml"))
            .arg("--project-directory")
            .arg(&self.repo_root)
            // Explicit project name: deriving it from /workspace would land
            // in a different compose project than the host's and collide on
            // the pinned container names (finding 2).
            .arg("-p")
            .arg(&self.project);
        if self.env_file.exists() {
            command.arg("--env-file").arg(&self.env_file);
        }
        command
            .arg("up")
            .arg("-d")
            // --no-deps: never touch the node dependencies. FALLBACK_FEE also
            // appears in the node commands, so without this a fee retune
            // could recreate the nodes and reset the chain.
            .arg("--no-deps")
            .arg("--force-recreate")
            .args(services)
            .current_dir(&self.repo_root);
        command
    }
}

impl Executor for SystemExecutor {
    fn compose_recreate(&self, services: &[String]) -> anyhow::Result<CommandOutput> {
        tracing::info!(services = services.join(","), "compose recreate");
        self.run(self.compose_command(services))
    }

    fn compose_recreate_with_env(
        &self,
        services: &[String],
        managed_env: &BTreeMap<String, String>,
    ) -> anyhow::Result<CommandOutput> {
        tracing::info!(services = services.join(","), "compose rollback recreate");
        let mut command = self.compose_command(services);
        scrub_managed_env(&mut command);
        command.envs(managed_env);
        self.run_raw(command)
    }

    fn remove_containers(&self, names: &[String]) -> anyhow::Result<CommandOutput> {
        if names.is_empty() {
            return Ok(CommandOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        let mut combined = CommandOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        };
        for name in names {
            let mut command = Command::new("docker");
            command
                .arg("rm")
                .arg("-f")
                .arg(name)
                .current_dir(&self.repo_root);
            let output = self.run(command)?;
            let missing = output
                .stderr
                .to_ascii_lowercase()
                .contains("no such container");
            combined.success &= output.success || missing;
            combined.stdout.push_str(&output.stdout);
            combined.stderr.push_str(&output.stderr);
        }
        Ok(combined)
    }

    fn inspect(&self, names: &[&str]) -> anyhow::Result<HashMap<String, ContainerInfo>> {
        let mut command = Command::new("docker");
        command
            .arg("inspect")
            .args(names)
            .current_dir(&self.repo_root);
        // docker inspect exits non-zero when ANY name is missing but still
        // prints the found ones. An empty result caused only by missing names
        // is valid; daemon/permission failures must reach callers.
        let output = self.run(command)?;
        if !output.success && output.stdout.trim().is_empty() {
            let stderr = output.stderr.to_ascii_lowercase();
            if !stderr.contains("no such object") && !stderr.contains("no such container") {
                anyhow::bail!("docker inspect failed: {}", output.stderr.trim());
            }
        }
        parse_inspect_output(&output.stdout)
    }

    fn node1_height(&self) -> anyhow::Result<u64> {
        let client = simchain_common::create_client(&self.node_urls[0])?;
        Ok(client.get_block_count()?)
    }

    fn spam_min_fee(&self) -> anyhow::Result<f64> {
        let mut required = 0.0_f64;
        for url in &self.node_urls {
            let client = simchain_common::create_client(url)?;
            let info = client.get_mempool_info()?;
            required = required
                .max(info.min_relay_tx_fee.to_btc())
                .max(info.mempool_min_fee.to_btc());
        }
        Ok(required)
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The panel's own environment must never leak managed values into child
/// processes: shell env overrides the project .env in compose interpolation,
/// so a stale value here would override the file the panel just rewrote
/// (finding 1). Unrelated variables (DOCKER_*, PATH, HOME, ...) pass through.
fn scrub_managed_env(command: &mut Command) {
    for spec in live_tuning::MANAGED_SETTINGS {
        command.env_remove(spec.key);
    }
    for alias in live_tuning::LEGACY_SPAM_ALIASES {
        command.env_remove(alias);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn executor_with(project: &str, dir: &std::path::Path) -> SystemExecutor {
        SystemExecutor::new(
            dir.to_path_buf(),
            dir.join(".env"),
            project.to_string(),
            vec![
                "http://node1:18443".to_string(),
                "http://node2:18443".to_string(),
                "http://node3:18443".to_string(),
            ],
        )
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn compose_command_pins_project_and_never_touches_deps() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".env"), "FALLBACK_FEE=0.0002\n").expect("write env");
        let executor = executor_with("customproj", dir.path());
        let command = executor.compose_command(&["btc-simnet-spammer".to_string()]);
        let args = args_of(&command);

        let p_index = args.iter().position(|a| a == "-p").expect("-p present");
        assert_eq!(args[p_index + 1], "customproj");
        assert!(args.contains(&"--no-deps".to_string()));
        assert!(args.contains(&"--force-recreate".to_string()));
        assert!(args.contains(&"btc-simnet-spammer".to_string()));
        // Explicit --env-file so the rewritten file wins over anything else.
        let e_index = args
            .iter()
            .position(|a| a == "--env-file")
            .expect("--env-file");
        assert!(args[e_index + 1].ends_with(".env"));
    }

    #[test]
    fn compose_command_skips_env_file_flag_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = executor_with("simchain", dir.path());
        let command = executor.compose_command(&[]);
        assert!(!args_of(&command).contains(&"--env-file".to_string()));
    }

    #[test]
    fn managed_env_is_scrubbed_from_child_process() {
        // Simulate a stale managed value in the panel's own environment: the
        // child must resolve the value from the file, not the process env.
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = executor_with("simchain", dir.path());
        let mut command = executor.compose_command(&[]);
        command.env("FALLBACK_FEE", "0.999");
        command.env("SPAM_TXS_PER_BLOCK", "7");
        command.env("DOCKER_HOST", "unix:///var/run/docker.sock");
        scrub_managed_env(&mut command);

        let envs: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
        let removed = |key: &str| {
            envs.iter()
                .any(|(k, v)| *k == OsStr::new(key) && v.is_none())
        };
        assert!(removed("FALLBACK_FEE"), "managed key must be removed");
        assert!(
            removed("SPAM_TXS_PER_BLOCK"),
            "legacy alias must be removed"
        );
        assert!(
            envs.iter()
                .any(|(k, v)| *k == OsStr::new("DOCKER_HOST") && v.is_some()),
            "unrelated variables must survive"
        );
    }
}
