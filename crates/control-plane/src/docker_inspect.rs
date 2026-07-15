//! Parsing of `docker inspect` output into the state and env the panel needs.

use crate::backend::ComponentInfo;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Clone, Debug, Deserialize)]
struct RawInspect {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "State")]
    state: RawState,
    #[serde(rename = "RestartCount", default)]
    restart_count: i64,
    #[serde(rename = "Config")]
    config: RawConfig,
}

#[derive(Clone, Debug, Deserialize)]
struct RawState {
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Running")]
    running: bool,
    #[serde(rename = "Restarting")]
    restarting: bool,
    #[serde(rename = "ExitCode", default)]
    exit_code: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct RawConfig {
    #[serde(rename = "Env", default)]
    env: Vec<String>,
}

/// One container's state and effective environment (keyed by container name
/// in the maps this module returns).
/// Parse `docker inspect <names...>` stdout. Docker prints a JSON array of
/// the containers it found (and exits non-zero listing the missing ones on
/// stderr); missing containers are simply absent from the result map.
pub fn parse_inspect_output(stdout: &str) -> anyhow::Result<HashMap<String, ComponentInfo>> {
    let stdout = stdout.trim();
    if stdout.is_empty() || stdout == "[]" {
        return Ok(HashMap::new());
    }
    let raw: Vec<RawInspect> = serde_json::from_str(stdout)?;
    Ok(raw
        .into_iter()
        .map(|entry| {
            let name = entry.name.trim_start_matches('/').to_string();
            let env = entry
                .config
                .env
                .iter()
                .filter_map(|pair| {
                    pair.split_once('=')
                        .map(|(key, value)| (key.to_string(), value.to_string()))
                })
                .collect();
            (
                name,
                ComponentInfo {
                    status: entry.state.status,
                    running: entry.state.running,
                    restarting: entry.state.restarting,
                    exit_code: entry.state.exit_code,
                    restart_count: entry.restart_count,
                    effective_config: env,
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state_and_env() {
        let stdout = r#"[
          {
            "Name": "/btc-simnet-spammer",
            "RestartCount": 2,
            "State": {
              "Status": "running",
              "Running": true,
              "Restarting": false,
              "ExitCode": 0,
              "StartedAt": "2026-07-12T00:00:00Z"
            },
            "Config": {
              "Env": ["ENABLE_SPAM=true", "FALLBACK_FEE=0.0001", "PATH=/usr/bin"]
            }
          }
        ]"#;
        let parsed = parse_inspect_output(stdout).expect("parse");
        let spammer = &parsed["btc-simnet-spammer"];
        assert!(spammer.running);
        assert_eq!(spammer.restart_count, 2);
        assert_eq!(spammer.effective_config["FALLBACK_FEE"], "0.0001");
    }

    #[test]
    fn empty_output_is_no_containers() {
        assert!(parse_inspect_output("").expect("parse").is_empty());
        assert!(parse_inspect_output("[]").expect("parse").is_empty());
    }
}
