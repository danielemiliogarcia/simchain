//! Shared test fixtures: a mock implementation of the domain backend ports
//! and an `AppState` builder. API/service tests never import Compose.

use crate::backend::{
    BackendOutput, ComponentBackend, ComponentInfo, ConfigurationBackend, JobActions,
    MiningControlBackend, SpamControlBackend,
};
use crate::control_state::ControlStateStore;
use crate::envfile;
use crate::state::{AppState, ControlPlaneConfig, CONTROLLER_CONTAINER, SPAMMER_CONTAINER};
use crate::status::StatusSnapshot;
use simchain_common::internal_api::{
    CommandAck, DesiredState, LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest,
    MiningWorkerStatus, PauseLease, SpamWorkerStatus, WorkerPhase,
};
use simchain_common::live_tuning;
use simchain_common::live_tuning::{MiningTuning, SpamTuning};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// What a service's container looks like after a mocked recreate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecreateOutcome {
    Running,
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
    pub mining_state: DesiredState,
    pub mining_generation: u64,
    pub spam_state: DesiredState,
    pub spam_generation: u64,
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
                mining_state: DesiredState::Running,
                mining_generation: 0,
                spam_state: DesiredState::Running,
                spam_generation: 0,
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
            RecreateOutcome::Crash => ("exited", false, false, 1, 0),
            RecreateOutcome::RestartLoop => ("restarting", false, true, 1, 2),
        };
        ComponentInfo {
            present: true,
            status: status.to_string(),
            running,
            restarting,
            exit_code,
            restart_count,
            effective_config: env,
            phase: None,
            effective_generation: None,
            uptime_secs: None,
            last_error: None,
            desired_state: None,
            effective_state: None,
            observed_height: None,
            next_scheduled_attempt_ms: None,
            last_mined_block: None,
            active_lease_count: None,
            cycle_phase: None,
            accepted_transactions: None,
            reconciliation_pending: None,
        }
    }

    /// Point-in-time world setup: both tool containers running with the env
    /// compose would currently render (i.e. running == staged).
    pub fn sync_containers(&self) {
        let env = self.rendered_env();
        let mining_env: HashMap<String, String> = MiningTuning::from_source(&env)
            .unwrap_or_else(|_| {
                MiningTuning::from_source(&live_tuning::staged_map(&BTreeMap::new()))
                    .expect("default mock mining policy")
            })
            .canonical_values()
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        let spam_env: HashMap<String, String> = SpamTuning::from_source(&env)
            .map(|(policy, _)| policy)
            .unwrap_or_else(|_| {
                SpamTuning::from_source(&live_tuning::staged_map(&BTreeMap::new()))
                    .expect("default mock spam policy")
                    .0
            })
            .canonical_values()
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        let mut world = self.world.lock().expect("world lock");
        world.containers.insert(
            CONTROLLER_CONTAINER.to_string(),
            Self::container_for(RecreateOutcome::Running, mining_env),
        );
        world.containers.insert(
            SPAMMER_CONTAINER.to_string(),
            Self::container_for(RecreateOutcome::Running, spam_env),
        );
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
}

impl ConfigurationBackend for MockBackend {
    fn apply_configuration(
        &self,
        services: &[String],
        desired: &BTreeMap<String, String>,
        generation: u64,
    ) -> anyhow::Result<BackendOutput> {
        if services
            .iter()
            .any(|service| service == CONTROLLER_CONTAINER)
        {
            let policy = MiningTuning::from_source(desired)?;
            MiningControlBackend::set_policy(self, generation, policy)?;
        }
        if services.iter().any(|service| service == SPAMMER_CONTAINER) {
            let (policy, _) = SpamTuning::from_source(desired)?;
            SpamControlBackend::set_policy(self, generation, policy)?;
        }
        Ok(BackendOutput {
            success: true,
            stdout: "applied policies through worker APIs".to_string(),
            stderr: String::new(),
        })
    }

    fn restore_configuration(
        &self,
        services: &[String],
        managed_env: &BTreeMap<String, String>,
        generations: &BTreeMap<String, u64>,
    ) -> anyhow::Result<BackendOutput> {
        if services
            .iter()
            .any(|service| service == CONTROLLER_CONTAINER)
        {
            let policy = MiningTuning::from_source(managed_env)?;
            MiningControlBackend::restore_policy(
                self,
                generations.get(CONTROLLER_CONTAINER).copied().unwrap_or(0),
                policy,
            )?;
        }
        if services.iter().any(|service| service == SPAMMER_CONTAINER) {
            let (policy, _) = SpamTuning::from_source(managed_env)?;
            SpamControlBackend::restore_policy(
                self,
                generations.get(SPAMMER_CONTAINER).copied().unwrap_or(0),
                policy,
            )?;
        }
        Ok(BackendOutput {
            success: true,
            stdout: "restored policies through worker APIs".to_string(),
            stderr: String::new(),
        })
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
                world.containers.get(*name).map(|info| {
                    let mut info = info.clone();
                    if *name == CONTROLLER_CONTAINER {
                        let phase = if world.mining_state == DesiredState::Paused {
                            WorkerPhase::Paused
                        } else {
                            WorkerPhase::Running
                        };
                        info.status = phase.as_str().to_string();
                        info.phase = Some(phase.as_str().to_string());
                        info.effective_generation = Some(world.mining_generation);
                        info.desired_state = Some(world.mining_state);
                        info.effective_state = Some(world.mining_state);
                        info.observed_height = Some(100);
                        info.uptime_secs = Some(1);
                        info.active_lease_count = Some(0);
                    } else if *name == SPAMMER_CONTAINER && info.running {
                        let enabled = SpamTuning::from_source(&info.effective_config)
                            .map(|(policy, _)| policy.enabled)
                            .unwrap_or(true);
                        let phase = if world.spam_state == DesiredState::Paused {
                            WorkerPhase::Paused
                        } else if enabled {
                            WorkerPhase::Active
                        } else {
                            WorkerPhase::Disabled
                        };
                        info.status = phase.as_str().to_string();
                        info.phase = Some(phase.as_str().to_string());
                        info.effective_generation = Some(world.spam_generation);
                        info.desired_state = Some(world.spam_state);
                        info.effective_state = Some(world.spam_state);
                        info.observed_height = Some(100);
                        info.uptime_secs = Some(1);
                        info.active_lease_count = Some(0);
                        info.cycle_phase = Some("waiting_for_block".to_string());
                        info.accepted_transactions = Some(10);
                        info.reconciliation_pending = Some(false);
                    }
                    (name.to_string(), info)
                })
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

impl MiningControlBackend for MockBackend {
    fn status(&self) -> anyhow::Result<MiningWorkerStatus> {
        let world = self.world.lock().expect("world lock");
        let component = world
            .containers
            .get(CONTROLLER_CONTAINER)
            .ok_or_else(|| anyhow::anyhow!("mining worker unavailable"))?;
        let policy = MiningTuning::from_source(&component.effective_config)?;
        Ok(MiningWorkerStatus {
            component: "mining".to_string(),
            phase: if world.mining_state == DesiredState::Paused {
                WorkerPhase::Paused
            } else {
                WorkerPhase::Running
            },
            desired_state: world.mining_state,
            effective_state: world.mining_state,
            effective_rng_seed: policy.rng_seed.unwrap_or(1),
            policy,
            effective_generation: world.mining_generation,
            height: Some(100),
            next_scheduled_attempt_ms: None,
            last_mined_block: None,
            active_leases: Vec::<PauseLease>::new(),
            uptime_secs: 1,
            last_error: None,
        })
    }

    fn set_state(&self, state: DesiredState) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        world.mining_state = state;
        Ok(CommandAck {
            request_id: "mock-state".to_string(),
            phase: if state == DesiredState::Paused {
                WorkerPhase::Paused
            } else {
                WorkerPhase::Running
            },
            effective_generation: world.mining_generation,
        })
    }

    fn set_policy(&self, generation: u64, policy: MiningTuning) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        let component = world
            .containers
            .get_mut(CONTROLLER_CONTAINER)
            .ok_or_else(|| anyhow::anyhow!("mining worker unavailable"))?;
        component.effective_config = policy
            .canonical_values()
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        component.effective_generation = Some(generation);
        world.mining_generation = generation;
        Ok(CommandAck {
            request_id: "mock-policy".to_string(),
            phase: WorkerPhase::Running,
            effective_generation: generation,
        })
    }

    fn restore_policy(&self, generation: u64, policy: MiningTuning) -> anyhow::Result<CommandAck> {
        MiningControlBackend::set_policy(self, generation, policy)
    }

    fn acquire_lease(&self, request: LeaseRequest) -> anyhow::Result<CommandAck> {
        MiningControlBackend::set_state(self, DesiredState::Paused).map(|mut ack| {
            ack.request_id = request.request_id;
            ack
        })
    }

    fn renew_lease(
        &self,
        _lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<CommandAck> {
        let status = MiningControlBackend::status(self)?;
        Ok(CommandAck {
            request_id: request.request_id,
            phase: status.phase,
            effective_generation: status.effective_generation,
        })
    }

    fn release_lease(
        &self,
        _lease_id: &str,
        request: LeaseReleaseRequest,
    ) -> anyhow::Result<CommandAck> {
        MiningControlBackend::set_state(self, DesiredState::Running).map(|mut ack| {
            ack.request_id = request.request_id;
            ack
        })
    }
}

impl SpamControlBackend for MockBackend {
    fn status(&self) -> anyhow::Result<SpamWorkerStatus> {
        let world = self.world.lock().expect("world lock");
        let component = world
            .containers
            .get(SPAMMER_CONTAINER)
            .filter(|component| component.running)
            .ok_or_else(|| anyhow::anyhow!("spam worker unavailable"))?;
        let (policy, _) = SpamTuning::from_source(&component.effective_config)?;
        let phase = if world.spam_state == DesiredState::Paused {
            WorkerPhase::Paused
        } else if policy.enabled {
            WorkerPhase::Active
        } else {
            WorkerPhase::Disabled
        };
        Ok(SpamWorkerStatus {
            component: "spam".to_string(),
            phase,
            desired_state: world.spam_state,
            effective_state: world.spam_state,
            policy,
            effective_generation: world.spam_generation,
            observed_height: Some(100),
            cycle_phase: Some("waiting_for_block".to_string()),
            accepted_transactions: 10,
            active_leases: Vec::<PauseLease>::new(),
            reconciliation_pending: false,
            uptime_secs: 1,
            last_error: None,
        })
    }

    fn set_state(&self, state: DesiredState) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        if !world.containers.contains_key(SPAMMER_CONTAINER) {
            anyhow::bail!("spam worker unavailable");
        }
        world.spam_state = state;
        Ok(CommandAck {
            request_id: "mock-spam-state".to_string(),
            phase: if state == DesiredState::Paused {
                WorkerPhase::Paused
            } else {
                WorkerPhase::Active
            },
            effective_generation: world.spam_generation,
        })
    }

    fn set_policy(&self, generation: u64, policy: SpamTuning) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        if world.kill_node1_on_recreate {
            world.node1_ok = false;
        }
        if world.compose_fail_times > 0 {
            world.compose_fail_times -= 1;
            anyhow::bail!("simulated worker policy failure");
        }
        if !world.containers.contains_key(SPAMMER_CONTAINER) {
            anyhow::bail!("spam worker unavailable");
        }
        let env = policy
            .canonical_values()
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        let outcome = world
            .after_recreate
            .remove(SPAMMER_CONTAINER)
            .unwrap_or(RecreateOutcome::Running);
        let mut component = Self::container_for(outcome, env);
        component.effective_generation = Some(generation);
        world
            .containers
            .insert(SPAMMER_CONTAINER.to_string(), component);
        world.spam_generation = generation;
        Ok(CommandAck {
            request_id: "mock-spam-policy".to_string(),
            phase: if policy.enabled {
                WorkerPhase::Active
            } else {
                WorkerPhase::Disabled
            },
            effective_generation: generation,
        })
    }

    fn restore_policy(&self, generation: u64, policy: SpamTuning) -> anyhow::Result<CommandAck> {
        SpamControlBackend::set_policy(self, generation, policy)
    }

    fn acquire_lease(&self, request: LeaseRequest) -> anyhow::Result<CommandAck> {
        SpamControlBackend::set_state(self, DesiredState::Paused).map(|mut ack| {
            ack.request_id = request.request_id;
            ack
        })
    }

    fn renew_lease(
        &self,
        _lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<CommandAck> {
        let status = SpamControlBackend::status(self)?;
        Ok(CommandAck {
            request_id: request.request_id,
            phase: status.phase,
            effective_generation: status.effective_generation,
        })
    }

    fn release_lease(
        &self,
        _lease_id: &str,
        request: LeaseReleaseRequest,
    ) -> anyhow::Result<CommandAck> {
        SpamControlBackend::set_state(self, DesiredState::Running).map(|mut ack| {
            ack.request_id = request.request_id;
            ack
        })
    }
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
            mining_control_url: "http://mock-mining:9081".to_string(),
            spam_control_url: "http://mock-spam:9082".to_string(),
            internal_token: "test-internal-token".to_string(),
        },
        token: "test-token".to_string(),
        components: backend.clone(),
        configuration: backend.clone(),
        job_actions: backend.clone(),
        mining: backend.clone(),
        spam: backend.clone(),
        control_state: RwLock::new(control_state),
        control_store: store,
        status: RwLock::new(StatusSnapshot::default()),
        apply_lock: Mutex::new(()),
    }
}
