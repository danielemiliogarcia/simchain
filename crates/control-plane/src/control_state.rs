//! Narrow, atomic control-plane desired-state storage and mutation lock.

use crate::storage;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use simchain_common::internal_api::DesiredState;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_SCHEMA_VERSION: u32 = 1;
const STATE_FILE: &str = "state.json";
const APPLY_LOCK_FILE: &str = "apply.lock";
const INSTANCE_LOCK_FILE: &str = "instance.lock";

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
        let source = simchain_common::live_tuning::staged_map(&BTreeMap::new());
        let (tuning, _) = simchain_common::live_tuning::LiveTuning::from_source(&source)
            .expect("built-in live-tuning defaults must remain valid");
        let desired = tuning
            .canonical_values()
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
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
        fs::create_dir_all(&dir)?;
        // Preserve ownership of the narrow host bind mount so its private
        // token remains readable to the host user.
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
        let _guard = self.try_apply_lock()?.ok_or_else(|| {
            anyhow::anyhow!("another control-plane process is initializing state")
        })?;
        match self.load() {
            Ok(state) => Ok(state),
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

    /// Reload the authoritative atomic state snapshot. Mutation callers hold
    /// the durable lock before using this value for a generation check.
    pub fn load_current(&self) -> anyhow::Result<ControlState> {
        Ok(self.load()?)
    }

    fn load(&self) -> std::io::Result<ControlState> {
        let content = fs::read_to_string(&self.path)?;
        let mut state: ControlState = serde_json::from_str(&content).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("control state {} is corrupt: {error}", self.path.display()),
            )
        })?;
        if state.schema_version != STATE_SCHEMA_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "unsupported control-state schema {} (expected {STATE_SCHEMA_VERSION})",
                    state.schema_version
                ),
            ));
        }
        migrate_legacy_desired(&mut state.desired);
        Ok(state)
    }

    pub fn save(&self, state: &ControlState) -> anyhow::Result<()> {
        let mut content = serde_json::to_string_pretty(state)?;
        content.push('\n');
        let ownership = storage::dir_ownership(&self.dir, 0o600)?;
        storage::write_atomic(&self.path, &content, ownership)?;
        Ok(())
    }

    pub fn try_apply_lock(&self) -> anyhow::Result<Option<File>> {
        self.try_named_lock(APPLY_LOCK_FILE)
    }

    pub fn try_instance_lock(&self) -> anyhow::Result<Option<File>> {
        self.try_named_lock(INSTANCE_LOCK_FILE)
    }

    fn try_named_lock(&self, name: &str) -> anyhow::Result<Option<File>> {
        let path = self.dir.join(name);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        let ownership = storage::dir_ownership(&self.dir, 0o600)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(ownership.mode))?;
        if let Err(error) =
            std::os::unix::fs::chown(&path, Some(ownership.uid), Some(ownership.gid))
        {
            tracing::debug!(path = %path.display(), "could not align lock ownership: {error}");
        }
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(file)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

/// Pre-split state stored the spam fee under `FALLBACK_FEE`, which now names
/// only the nodes' boot-time flag and is no longer a managed key. Carry the
/// value over so an upgrade keeps the running spam fee.
fn migrate_legacy_desired(desired: &mut BTreeMap<String, String>) {
    if let Some(value) = desired.remove("FALLBACK_FEE") {
        desired.entry("SPAM_FEE".to_string()).or_insert(value);
    }
    // The engine toggle is pinned to the raw engine and no longer managed.
    desired.remove("USE_RAW_TX_SPAM");
}

/// Initial desired values are the boot policy passed to the workers and
/// control plane. Once state.json exists it is authoritative.
pub fn desired_from_process_env() -> anyhow::Result<BTreeMap<String, String>> {
    let mut overrides: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| simchain_common::live_tuning::is_managed_key(key))
        .collect();
    // Legacy boot environment: before the split FALLBACK_FEE also set the
    // spam fee. Seed SPAM_FEE from it so the first state.json keeps the fee.
    if overrides
        .get("SPAM_FEE")
        .is_none_or(|v| v.trim().is_empty())
    {
        if let Ok(value) = std::env::var("FALLBACK_FEE") {
            if !value.trim().is_empty() {
                overrides.insert("SPAM_FEE".to_string(), value);
            }
        }
    }
    desired_from_source(&overrides)
}

fn desired_from_source(
    overrides: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let source = simchain_common::live_tuning::staged_map(overrides);
    let (tuning, _) = simchain_common::live_tuning::LiveTuning::from_source(&source)?;
    Ok(tuning
        .canonical_values()
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect())
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
    fn legacy_fallback_fee_state_migrates_to_spam_fee_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ControlStateStore::open(dir.path().to_path_buf()).expect("store");
        let mut legacy = ControlState::default();
        legacy.desired.remove("SPAM_FEE");
        legacy
            .desired
            .insert("FALLBACK_FEE".to_string(), "0.0005".to_string());
        legacy
            .desired
            .insert("USE_RAW_TX_SPAM".to_string(), "false".to_string());
        store.save(&legacy).expect("save legacy state");

        let loaded = store.load_current().expect("load");
        assert!(!loaded.desired.contains_key("FALLBACK_FEE"));
        assert_eq!(loaded.desired["SPAM_FEE"], "0.0005");
        assert!(!loaded.desired.contains_key("USE_RAW_TX_SPAM"));
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

    #[test]
    fn initial_desired_values_are_canonicalized_from_boot_policy() {
        let desired = desired_from_source(&BTreeMap::from([
            ("BLOCK_INTERVAL_MEAN_SECS".to_string(), "12".to_string()),
            ("MINER_WEIGHTS".to_string(), "70, 30".to_string()),
        ]))
        .expect("desired");
        assert_eq!(desired["BLOCK_INTERVAL_MEAN_SECS"], "12");
        assert_eq!(desired["MINER_WEIGHTS"], "70,30");
    }

    #[test]
    fn apply_lock_is_exclusive_and_lives_in_the_state_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ControlStateStore::open(dir.path().join("state")).expect("store");
        let first = store.try_apply_lock().expect("lock").expect("first lock");
        assert!(store.try_apply_lock().expect("second attempt").is_none());
        assert_eq!(
            fs::metadata(store.dir.join(APPLY_LOCK_FILE))
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(first);
        assert!(store.try_apply_lock().expect("third attempt").is_some());
    }

    #[test]
    fn instance_lock_enforces_the_single_control_plane_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ControlStateStore::open(dir.path().join("state")).expect("store");
        let first = store
            .try_instance_lock()
            .expect("lock")
            .expect("first instance");
        assert!(store.try_instance_lock().expect("second attempt").is_none());
        drop(first);
        assert!(store.try_instance_lock().expect("third attempt").is_some());
    }
}
