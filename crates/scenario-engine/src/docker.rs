use anyhow::{bail, Context, Result};
use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::{Command, Output},
};

pub const MINING_CONTROLLER: &str = "btc-simnet-mining-controller";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: PathBuf,
}

impl CommandSpec {
    fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        current_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            current_dir: current_dir.into(),
        }
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, command: &CommandSpec) -> Result<Output>;
}

pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &CommandSpec) -> Result<Output> {
        Command::new(&command.program)
            .args(&command.args)
            .current_dir(&command.current_dir)
            .output()
            .with_context(|| format!("failed to launch {}", command.program.display()))
    }
}

pub struct Docker {
    repo_root: PathBuf,
    runner: Box<dyn CommandRunner>,
}

impl Docker {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            runner: Box::new(SystemCommandRunner),
        }
    }

    #[cfg(test)]
    pub fn with_runner(repo_root: PathBuf, runner: Box<dyn CommandRunner>) -> Self {
        Self { repo_root, runner }
    }

    pub fn pause_mining(&self) -> Result<()> {
        self.run_checked(&self.compose(["stop", MINING_CONTROLLER]))
    }

    pub fn resume_mining(&self) -> Result<()> {
        self.run_checked(&self.compose(["up", "-d", MINING_CONTROLLER]))
    }

    pub fn container_running(&self, container: &str) -> Result<bool> {
        let command = CommandSpec::new(
            "docker",
            ["inspect", "-f", "{{.State.Running}}", container],
            &self.repo_root,
        );
        let output = self.runner.run(&command)?;
        Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
    }

    pub fn reorg(&self, depth: u64, empty: bool) -> Result<()> {
        self.run_checked(&self.reorg_command(depth, empty))
    }

    pub fn partition(&self, node: &str, main_blocks: u64, isolated_blocks: u64) -> Result<()> {
        self.run_checked(&self.partition_command(node, main_blocks, isolated_blocks))
    }

    pub fn heal_partition(&self, node: &str) -> Result<()> {
        self.run_checked(&CommandSpec::new(
            self.repo_root.join("scripts/partition.sh"),
            [OsString::from("heal"), OsString::from(node)],
            &self.repo_root,
        ))
    }

    fn compose<I, S>(&self, args: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut all_args = vec![
            OsString::from("compose"),
            OsString::from("-f"),
            self.repo_root.join("docker-compose.yml").into_os_string(),
            OsString::from("--project-directory"),
            self.repo_root.clone().into_os_string(),
        ];
        all_args.extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        CommandSpec::new("docker", all_args, &self.repo_root)
    }

    fn reorg_command(&self, depth: u64, empty: bool) -> CommandSpec {
        let mut args = vec![OsString::from(depth.to_string())];
        if empty {
            args.push(OsString::from("empty"));
        }
        CommandSpec::new(
            self.repo_root.join("scripts/simulate-reorg.sh"),
            args,
            &self.repo_root,
        )
    }

    fn partition_command(&self, node: &str, main_blocks: u64, isolated_blocks: u64) -> CommandSpec {
        CommandSpec::new(
            self.repo_root.join("scripts/partition.sh"),
            [
                OsString::from("run"),
                OsString::from(node),
                OsString::from("--main-blocks"),
                OsString::from(main_blocks.to_string()),
                OsString::from("--isolated-blocks"),
                OsString::from(isolated_blocks.to_string()),
            ],
            &self.repo_root,
        )
    }

    fn run_checked(&self, command: &CommandSpec) -> Result<()> {
        tracing::info!(
            command = %format_command(command),
            "Running orchestration command"
        );
        let output = self.runner.run(command)?;
        if !output.stdout.is_empty() {
            tracing::info!("{}", String::from_utf8_lossy(&output.stdout).trim_end());
        }
        if !output.stderr.is_empty() {
            tracing::info!("{}", String::from_utf8_lossy(&output.stderr).trim_end());
        }
        if !output.status.success() {
            bail!(
                "command failed with {}: {}",
                output.status,
                format_command(command)
            );
        }
        Ok(())
    }
}

fn format_command(command: &CommandSpec) -> String {
    std::iter::once(command.program.as_os_str())
        .chain(command.args.iter().map(OsString::as_os_str))
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        os::unix::process::ExitStatusExt,
        path::Path,
        sync::{Arc, Mutex},
    };

    struct Recorder {
        commands: Arc<Mutex<Vec<CommandSpec>>>,
        outputs: Mutex<VecDeque<Output>>,
    }

    impl CommandRunner for Recorder {
        fn run(&self, command: &CommandSpec) -> Result<Output> {
            self.commands.lock().unwrap().push(command.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap_or(Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            }))
        }
    }

    fn docker() -> (Docker, Arc<Mutex<Vec<CommandSpec>>>) {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let runner = Recorder {
            commands: Arc::clone(&commands),
            outputs: Mutex::new(VecDeque::new()),
        };
        (
            Docker::with_runner(PathBuf::from("/workspace"), Box::new(runner)),
            commands,
        )
    }

    #[test]
    fn builds_reorg_wrapper_command() {
        let (docker, commands) = docker();
        docker.reorg(3, true).unwrap();
        let command = &commands.lock().unwrap()[0];
        assert_eq!(
            command.program,
            Path::new("/workspace/scripts/simulate-reorg.sh")
        );
        assert_eq!(command.args, ["3", "empty"]);
    }

    #[test]
    fn builds_partition_wrapper_command() {
        let (docker, commands) = docker();
        docker.partition("btc-simnet-node3", 3, 4).unwrap();
        let command = &commands.lock().unwrap()[0];
        assert_eq!(
            command.program,
            Path::new("/workspace/scripts/partition.sh")
        );
        assert_eq!(
            command.args,
            [
                "run",
                "btc-simnet-node3",
                "--main-blocks",
                "3",
                "--isolated-blocks",
                "4"
            ]
        );
    }
}
