//! Atomic job metadata plus bounded JSONL event artifacts.

use crate::envfile;
use serde::{de::DeserializeOwned, Serialize};
use simchain_common::control_api::JobEvent;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const INDEX_FILE: &str = "index.json";
const EVENT_FILE_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct JobStore {
    dir: PathBuf,
    index_path: PathBuf,
}

impl JobStore {
    pub fn open(state_dir: &Path) -> anyhow::Result<Self> {
        let dir = state_dir.join("jobs");
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        if let Ok(ownership) = envfile::dir_ownership(state_dir, 0o700) {
            let _ = std::os::unix::fs::chown(&dir, Some(ownership.uid), Some(ownership.gid));
        }
        Ok(Self {
            index_path: dir.join(INDEX_FILE),
            dir,
        })
    }

    pub fn load<T: DeserializeOwned + Default>(&self) -> anyhow::Result<T> {
        match fs::read_to_string(&self.index_path) {
            Ok(content) => serde_json::from_str(&content).map_err(|error| {
                anyhow::anyhow!(
                    "job index {} is corrupt: {error}",
                    self.index_path.display()
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save<T: Serialize>(&self, value: &T) -> anyhow::Result<()> {
        let mut content = serde_json::to_string_pretty(value)?;
        content.push('\n');
        let ownership = envfile::dir_ownership(&self.dir, 0o600)?;
        envfile::write_atomic(&self.index_path, &content, ownership)?;
        Ok(())
    }

    pub fn append_event(&self, event: &JobEvent) -> anyhow::Result<()> {
        let path = self.event_path(&event.job_id)?;
        if fs::metadata(&path)
            .map(|metadata| metadata.len() >= EVENT_FILE_LIMIT)
            .unwrap_or(false)
        {
            let rotated = self.rotated_event_path(&event.job_id)?;
            if rotated.exists() {
                fs::remove_file(&rotated)?;
            }
            fs::rename(&path, rotated)?;
        }

        let existed = path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        serde_json::to_writer(&mut file, event)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        if !existed {
            let ownership = envfile::dir_ownership(&self.dir, 0o600)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(ownership.mode))?;
            let _ = std::os::unix::fs::chown(&path, Some(ownership.uid), Some(ownership.gid));
        }
        Ok(())
    }

    pub fn read_events(&self, job_id: &str) -> anyhow::Result<Vec<JobEvent>> {
        let mut events = Vec::new();
        for path in [self.rotated_event_path(job_id)?, self.event_path(job_id)?] {
            let content = match fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for (index, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let event: JobEvent = serde_json::from_str(line).map_err(|error| {
                    anyhow::anyhow!(
                        "job event file {} line {} is corrupt: {error}",
                        path.display(),
                        index + 1
                    )
                })?;
                events.push(event);
            }
        }
        events.sort_by_key(|event| event.sequence);
        events.dedup_by_key(|event| event.sequence);
        Ok(events)
    }

    pub fn remove_events(&self, job_id: &str) -> anyhow::Result<()> {
        for path in [self.event_path(job_id)?, self.rotated_event_path(job_id)?] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn event_path(&self, job_id: &str) -> anyhow::Result<PathBuf> {
        validate_job_id(job_id)?;
        Ok(self.dir.join(format!("{job_id}.jsonl")))
    }

    fn rotated_event_path(&self, job_id: &str) -> anyhow::Result<PathBuf> {
        validate_job_id(job_id)?;
        Ok(self.dir.join(format!("{job_id}.jsonl.1")))
    }
}

fn validate_job_id(job_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !job_id.is_empty()
            && job_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid job ID"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use simchain_common::control_api::JobEvent;

    #[test]
    fn metadata_and_events_round_trip_privately() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JobStore::open(dir.path()).expect("store");
        let value = serde_json::json!({"schema_version": 1, "jobs": []});
        store.save(&value).expect("save");
        assert_eq!(store.load::<serde_json::Value>().expect("load"), value);
        let event = JobEvent {
            sequence: 7,
            job_id: "job-1".to_string(),
            timestamp_ms: 10,
            event: "started".to_string(),
            phase: "starting".to_string(),
            message: "started".to_string(),
            data: None,
        };
        store.append_event(&event).expect("append");
        assert_eq!(store.read_events("job-1").expect("events"), vec![event]);
        assert_eq!(
            fs::metadata(dir.path().join("jobs/index.json"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn job_ids_cannot_escape_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JobStore::open(dir.path()).expect("store");
        assert!(store.read_events("../outside").is_err());
    }
}
