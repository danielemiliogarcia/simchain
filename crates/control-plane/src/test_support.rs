//! Shared test fixtures: a mock implementation of the domain backend ports
//! and an `AppState` builder. API/service tests never import Compose.

use crate::backend::{
    BackendOutput, ComponentBackend, ComponentInfo, ConfigurationBackend, JobActions,
};
use crate::control_state::ControlStateStore;
use crate::envfile;
use crate::state::{AppState, ControlPlaneConfig, CONTROLLER_CONTAINER, SPAMMER_CONTAINER};
use crate::status::StatusSnapshot;
use simchain_common::live_tuning;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// What a service's container looks like after a mocked recreate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecreateOutcome {
    Running,
    ExitedClean,
    Crash,
    RestartLoop,
}

pub struct MockWorld {
    pub containers: HashMap<String, ComponentInfo>,
    pub compose_calls: Vec<Vec<String>>,
    /// Fail the next N compose invocations (exit non-zero).
    pub compose_fail_times: u32,
    pub after_recreate: HashMap<String, RecreateOutcome>,
    pub node1_ok: bool,
    /// Simulate the recreate taking node1 down (exercises the RPC guard).
    pub kill_node1_on_recreate: bool,
}

pub struct MockBackend {
    pub env_file: PathBuf,
    pub min_relay: f64,
    pub world: Mutex<MockWorld>,
}

impl MockBackend {
    pub fn new(env_file: PathBuf) -> Self {
        Self {
            env_file,
            min_relay: 0.00001,
            world: Mutex::new(MockWorld {
                containers: HashMap::new(),
                compose_calls: Vec::new(),
                compose_fail_times: 0,
                after_recreate: HashMap::new(),
                node1_ok: true,
                kill_node1_on_recreate: false,
            }),
        }
    }

    /// The container env compose would render right now: `.env` overlaid on
    /// the compose defaults (which equal the catalog defaults).
    pub fn rendered_env(&self) -> HashMap<String, String> {
        let content = std::fs::read_to_string(&self.env_file).unwrap_or_default();
        let overrides = envfile::managed_overrides(&content);
        live_tuning::staged_map(&overrides).into_iter().collect()
    }

    fn container_for(outcome: RecreateOutcome, env: HashMap<String, String>) -> ComponentInfo {
        let (status, running, restarting, exit_code, restart_count) = match outcome {
            RecreateOutcome::Running => ("running", true, false, 0, 0),
            RecreateOutcome::ExitedClean => ("exited", false, false, 0, 0),
            RecreateOutcome::Crash => ("exited", false, false, 1, 0),
            RecreateOutcome::RestartLoop => ("restarting", false, true, 1, 2),
        };
        ComponentInfo {
            status: status.to_string(),
            running,
            restarting,
            exit_code,
            restart_count,
            effective_config: env,
        }
    }

    /// Point-in-time world setup: both tool containers running with the env
    /// compose would currently render (i.e. running == staged).
    pub fn sync_containers(&self) {
        let env = self.rendered_env();
        let mut world = self.world.lock().expect("world lock");
        for name in [CONTROLLER_CONTAINER, SPAMMER_CONTAINER] {
            world.containers.insert(
                name.to_string(),
                Self::container_for(RecreateOutcome::Running, env.clone()),
            );
        }
    }

    pub fn set_container_env(&self, name: &str, key: &str, value: &str) {
        let mut world = self.world.lock().expect("world lock");
        let container = world.containers.get_mut(name).expect("container exists");
        container
            .effective_config
            .insert(key.to_string(), value.to_string());
    }

    pub fn compose_calls(&self) -> Vec<Vec<String>> {
        self.world.lock().expect("world lock").compose_calls.clone()
    }

    fn recreate_with_env(
        &self,
        services: &[String],
        env: HashMap<String, String>,
    ) -> anyhow::Result<BackendOutput> {
        let mut world = self.world.lock().expect("world lock");
        world.compose_calls.push(services.to_vec());
        if world.kill_node1_on_recreate {
            world.node1_ok = false;
        }
        if world.compose_fail_times > 0 {
            world.compose_fail_times -= 1;
            return Ok(BackendOutput {
                success: false,
                stdout: String::new(),
                stderr: "simulated compose failure".to_string(),
            });
        }
        for service in services {
            let outcome = world
                .after_recreate
                .remove(service)
                .unwrap_or(RecreateOutcome::Running);
            world
                .containers
                .insert(service.clone(), Self::container_for(outcome, env.clone()));
        }
        Ok(BackendOutput {
            success: true,
            stdout: format!("recreated {}", services.join(",")),
            stderr: String::new(),
        })
    }
}

impl ConfigurationBackend for MockBackend {
    fn apply_configuration(&self, services: &[String]) -> anyhow::Result<BackendOutput> {
        let env = {
            // Read the file the apply just wrote, exactly like compose would.
            let content = std::fs::read_to_string(&self.env_file).unwrap_or_default();
            let overrides = envfile::managed_overrides(&content);
            live_tuning::staged_map(&overrides)
                .into_iter()
                .collect::<HashMap<_, _>>()
        };
        self.recreate_with_env(services, env)
    }

    fn restore_configuration(
        &self,
        services: &[String],
        managed_env: &BTreeMap<String, String>,
    ) -> anyhow::Result<BackendOutput> {
        self.recreate_with_env(services, managed_env.clone().into_iter().collect())
    }

    fn remove_components(&self, names: &[String]) -> anyhow::Result<BackendOutput> {
        let mut world = self.world.lock().expect("world lock");
        for name in names {
            world.containers.remove(name);
        }
        Ok(BackendOutput {
            success: true,
            stdout: names.join("\n"),
            stderr: String::new(),
        })
    }
}

impl ComponentBackend for MockBackend {
    fn inspect_components(&self, names: &[&str]) -> anyhow::Result<HashMap<String, ComponentInfo>> {
        let world = self.world.lock().expect("world lock");
        Ok(names
            .iter()
            .filter_map(|name| {
                world
                    .containers
                    .get(*name)
                    .map(|info| (name.to_string(), info.clone()))
            })
            .collect())
    }
}

impl JobActions for MockBackend {
    fn node1_height(&self) -> anyhow::Result<u64> {
        if self.world.lock().expect("world lock").node1_ok {
            Ok(100)
        } else {
            anyhow::bail!("node1 unreachable")
        }
    }

    fn spam_min_fee(&self) -> anyhow::Result<f64> {
        if self.world.lock().expect("world lock").node1_ok {
            Ok(self.min_relay)
        } else {
            anyhow::bail!("spam nodes unreachable")
        }
    }

    fn wait(&self, _duration: Duration) {}
}

pub fn test_app(dir: &Path, backend: Arc<MockBackend>) -> AppState {
    let state_dir = dir.join(".simchain-control");
    let store = ControlStateStore::open(state_dir.clone()).expect("control store");
    let desired =
        crate::control_state::desired_from_legacy_env(&dir.join(".env")).expect("initial desired");
    let control_state = store.load_or_initialize(desired).expect("control state");
    AppState {
        config: ControlPlaneConfig {
            listen_addr: "127.0.0.1:0".parse().expect("addr"),
            repo_root: dir.to_path_buf(),
            env_file: dir.join(".env"),
            compose_project: "simchain".to_string(),
            node1_url: "http://mock-node1:18443".to_string(),
            node2_url: "http://mock-node2:18443".to_string(),
            node3_url: "http://mock-node3:18443".to_string(),
            state_dir,
        },
        token: "test-token".to_string(),
        components: backend.clone(),
        configuration: backend.clone(),
        job_actions: backend,
        control_state: RwLock::new(control_state),
        control_store: store,
        status: RwLock::new(StatusSnapshot::default()),
        apply_lock: Mutex::new(()),
    }
}
