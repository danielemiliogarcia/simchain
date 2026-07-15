//! Narrow, atomic control-plane state storage. Phase 1 persists the API token
//! and configuration generation here while the legacy Compose adapter still
//! mirrors desired values into `.env`.

use crate::envfile;
use serde::{Deserialize, Serialize};
use simchain_common::internal_api::DesiredState;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_SCHEMA_VERSION: u32 = 1;
const STATE_FILE: &str = "state.json";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ApplyOutcome {
    pub status: String,
    pub updated_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ControlState {
    pub schema_version: u32,
    pub generation: u64,
    pub desired: BTreeMap<String, String>,
    #[serde(default = "running_state")]
    pub mining_state: DesiredState,
    #[serde(default = "running_state")]
    pub spam_state: DesiredState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_apply: Option<ApplyOutcome>,
}

impl Default for ControlState {
    fn default() -> Self {
        let desired = simchain_common::live_tuning::staged_map(&BTreeMap::new());
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            generation: 0,
            desired,
            mining_state: DesiredState::Running,
            spam_state: DesiredState::Running,
            last_apply: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ControlStateStore {
    dir: PathBuf,
    path: PathBuf,
}

impl ControlStateStore {
    pub fn open(dir: PathBuf) -> anyhow::Result<Self> {
        let parent_ownership = dir
            .parent()
            .and_then(|parent| envfile::dir_ownership(parent, 0o700).ok());
        fs::create_dir_all(&dir)?;
        if let Some(ownership) = parent_ownership {
            if let Err(error) =
                std::os::unix::fs::chown(&dir, Some(ownership.uid), Some(ownership.gid))
            {
                tracing::debug!("could not align control-state ownership: {error}");
            }
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            path: dir.join(STATE_FILE),
            dir,
        })
    }

    pub fn load_or_initialize(
        &self,
        initial_desired: BTreeMap<String, String>,
    ) -> anyhow::Result<ControlState> {
        match fs::read_to_string(&self.path) {
            Ok(content) => {
                let state: ControlState = serde_json::from_str(&content).map_err(|error| {
                    anyhow::anyhow!("control state {} is corrupt: {error}", self.path.display())
                })?;
                if state.schema_version != STATE_SCHEMA_VERSION {
                    anyhow::bail!(
                        "unsupported control-state schema {} (expected {})",
                        state.schema_version,
                        STATE_SCHEMA_VERSION
                    );
                }
                Ok(state)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let state = ControlState {
                    desired: initial_desired,
                    ..ControlState::default()
                };
                self.save(&state)?;
                Ok(state)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, state: &ControlState) -> anyhow::Result<()> {
        let mut content = serde_json::to_string_pretty(state)?;
        content.push('\n');
        let ownership = envfile::dir_ownership(&self.dir, 0o600)?;
        envfile::write_atomic(&self.path, &content, ownership)?;
        Ok(())
    }
}

/// Snapshot the transitional env-backed desired configuration for a first
/// state-file initialization. Once state.json exists it always wins here.
pub fn desired_from_legacy_env(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let file = envfile::read_env_file(path)?;
    let staged = crate::service::staged_from_content(&file.content);
    Ok(match staged.tuning {
        Ok((tuning, _)) => tuning
            .canonical_values()
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
        Err(_) => crate::service::tuning_source(&staged.overrides),
    })
}

pub fn successful_apply(
    previous: &ControlState,
    desired: BTreeMap<String, String>,
) -> ControlState {
    let changed = previous.desired != desired;
    ControlState {
        schema_version: STATE_SCHEMA_VERSION,
        generation: previous.generation + u64::from(changed),
        desired,
        mining_state: previous.mining_state,
        spam_state: previous.spam_state,
        last_apply: Some(ApplyOutcome {
            status: "succeeded".to_string(),
            updated_at_ms: now_ms(),
            message: None,
        }),
    }
}

fn running_state() -> DesiredState {
    DesiredState::Running
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_private_versioned_state_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_dir = dir.path().join("state");
        let store = ControlStateStore::open(state_dir.clone()).expect("store");
        let initial = store
            .load_or_initialize(ControlState::default().desired)
            .expect("initialize");
        assert_eq!(initial.schema_version, 1);
        assert_eq!(initial.generation, 0);

        let mut desired = initial.desired.clone();
        desired.insert("BLOCK_INTERVAL_MEAN_SECS".to_string(), "12".to_string());
        let next = successful_apply(&initial, desired);
        store.save(&next).expect("save");
        assert_eq!(
            store.load_or_initialize(BTreeMap::new()).expect("reload"),
            next
        );
        assert_eq!(
            fs::metadata(state_dir.join(STATE_FILE))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn corrupt_state_fails_visibly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ControlStateStore::open(dir.path().to_path_buf()).expect("store");
        fs::write(dir.path().join(STATE_FILE), "not json").expect("seed");
        let error = store
            .load_or_initialize(BTreeMap::new())
            .expect_err("corrupt state");
        assert!(error.to_string().contains("is corrupt"));
    }
}
