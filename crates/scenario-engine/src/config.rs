use anyhow::{bail, Context, Result};
use simchain_common::config::{
    DEFAULT_NODE1_RPC_URL, DEFAULT_NODE2_RPC_URL, DEFAULT_NODE2_WALLET_NAME, DEFAULT_NODE3_RPC_URL,
    DEFAULT_NODE3_WALLET_NAME,
};
use std::{env, path::PathBuf, time::Duration};

#[derive(Clone, Debug)]
pub struct Config {
    pub scenario_file: PathBuf,
    pub repo_root: PathBuf,
    pub result_file: Option<PathBuf>,
    pub timeout: Duration,
    pub node1_url: String,
    pub node2_url: String,
    pub node3_url: String,
    pub node2_wallet: String,
    pub node3_wallet: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let repo_root = PathBuf::from(env_or("SIMCHAIN_REPO_ROOT", "/workspace"));
        let scenario_file = resolve_path(
            &repo_root,
            PathBuf::from(env_or(
                "SCENARIO_FILE",
                "/workspace/scenarios/pause-then-burst.yml",
            )),
        );
        let result_file = env::var("SCENARIO_RESULT_FILE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .map(|path| resolve_path(&repo_root, path));
        let timeout_secs = env_or("SCENARIO_TIMEOUT_SECS", "1800")
            .parse::<u64>()
            .context("SCENARIO_TIMEOUT_SECS must be a positive integer")?;
        if timeout_secs == 0 {
            bail!("SCENARIO_TIMEOUT_SECS must be a positive integer");
        }

        Ok(Self {
            scenario_file,
            repo_root,
            result_file,
            timeout: Duration::from_secs(timeout_secs),
            node1_url: env_or("NODE1_RPC_URL", DEFAULT_NODE1_RPC_URL),
            node2_url: env_or("NODE2_RPC_URL", DEFAULT_NODE2_RPC_URL),
            node3_url: env_or("NODE3_RPC_URL", DEFAULT_NODE3_RPC_URL),
            node2_wallet: env_or("NODE2_WALLET_NAME", DEFAULT_NODE2_WALLET_NAME),
            node3_wallet: env_or("NODE3_WALLET_NAME", DEFAULT_NODE3_WALLET_NAME),
        })
    }
}

fn resolve_path(root: &std::path::Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}
