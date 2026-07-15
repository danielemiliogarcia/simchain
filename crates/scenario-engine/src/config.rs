use anyhow::{bail, Context, Result};
use simchain_common::control_api::DEFAULT_CONTROL_URL;
use std::{env, path::PathBuf, time::Duration};

#[derive(Clone, Debug)]
pub struct Config {
    pub scenario_file: PathBuf,
    pub result_file: Option<PathBuf>,
    pub timeout: Duration,
    pub control_url: String,
    pub token: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let scenario_file =
            PathBuf::from(env_or("SCENARIO_FILE", "scenarios/pause-then-burst.yml"));
        let result_file = env::var("SCENARIO_RESULT_FILE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
        let timeout_secs = env_or("SCENARIO_TIMEOUT_SECS", "1800")
            .parse::<u64>()
            .context("SCENARIO_TIMEOUT_SECS must be a positive integer")?;
        if timeout_secs == 0 {
            bail!("SCENARIO_TIMEOUT_SECS must be a positive integer");
        }
        let state_dir = env::var("SIMCHAIN_CONTROL_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(".simchain-control"));
        let configured_token = env::var("SIMCHAIN_CONTROL_TOKEN")
            .ok()
            .filter(|token| !token.trim().is_empty())
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty());
        let token = match configured_token {
            Some(token) => token,
            None => wait_for_token(&state_dir.join("token"))?,
        };

        Ok(Self {
            scenario_file,
            result_file,
            timeout: Duration::from_secs(timeout_secs),
            control_url: env_or("SIMCHAIN_CONTROL_URL", DEFAULT_CONTROL_URL)
                .trim_end_matches('/')
                .to_string(),
            token,
        })
    }
}

fn wait_for_token(path: &std::path::Path) -> Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(token) = std::fs::read_to_string(path)
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
        {
            return Ok(token);
        }
        if std::time::Instant::now() >= deadline {
            bail!("control-plane token is unavailable at {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}
