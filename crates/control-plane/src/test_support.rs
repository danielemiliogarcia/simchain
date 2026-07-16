//! Shared domain-level test fixtures and an `AppState` builder.

use crate::backend::{
    ChainBackend, MiningControlBackend, NetworkControlBackend, SpamControlBackend,
};
use crate::control_state::{ControlState, ControlStateStore};
use crate::faucet_job::{
    FaucetBackend, FaucetConfirmation, FaucetInput, FaucetPreflight, MinerVerification,
    PreparedFaucetTransaction, PriorityUpdate,
};
use crate::jobs::{FaucetSettings, JobDependencies, JobManager};
use crate::network_job::{ChainSnapshot, NetworkActionBackend};
use crate::reorg_job::{ReorgExecution, ReorgExecutor, ReorgRecoveryContext};
use crate::scenario_job::ScenarioActionBackend;
use crate::state::{AppState, ControlPlaneConfig, MINING_COMPONENT, SPAM_COMPONENT};
use crate::status::StatusSnapshot;
use simchain_common::control_api::{FaucetOutput, FaucetSourceNode, FAUCET_PRIORITY_DELTA_SATS};
use simchain_common::internal_api::{
    CommandAck, DesiredState, LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest,
    MiningWorkerStatus, NetworkAgentStatus, NetworkCommandAck, NetworkImpairmentLease,
    NetworkLeaseReleaseRequest, NetworkLeaseRequest, PauseLease, SpamWorkerStatus, WorkerPhase,
};
use simchain_common::live_tuning::{self, MiningTuning, SpamTuning};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

struct MockReorgExecutor;

impl ReorgExecutor for MockReorgExecutor {
    fn execute(
        &self,
        request: &simchain_common::control_api::ReorgJobRequest,
        _use_raw_tx_spam: bool,
        observer: &dyn simchain_reorg::ReorgObserver,
    ) -> anyhow::Result<ReorgExecution> {
        observer.observe(simchain_reorg::ReorgProgress {
            phase: simchain_reorg::ReorgPhase::Completed,
            message: "mock reorg completed".to_string(),
            data: None,
        });
        Ok(ReorgExecution {
            result: serde_json::json!({"depth": request.depth, "chain_changed": true}),
            chain_changed: true,
            aborted: observer.abort_requested(),
        })
    }

    fn recover(
        &self,
        _request: &simchain_common::control_api::ReorgJobRequest,
        _context: &ReorgRecoveryContext,
        _observer: &dyn simchain_reorg::ReorgObserver,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct MockWorld {
    pub mining_available: bool,
    pub spam_available: bool,
    pub node1_ok: bool,
    pub mining_state: DesiredState,
    pub mining_generation: u64,
    pub mining_policy: MiningTuning,
    pub mining_leases: HashMap<String, PauseLease>,
    pub spam_state: DesiredState,
    pub spam_generation: u64,
    pub spam_policy: SpamTuning,
    pub spam_leases: HashMap<String, PauseLease>,
    pub mining_policy_fail_times: u32,
    pub spam_policy_fail_times: u32,
    pub mining_restore_fail_times: u32,
    pub spam_restore_fail_times: u32,
    pub kill_node1_on_policy: bool,
    pub mining_status_fail_times: u32,
    pub spam_status_fail_times: u32,
    pub network_leases: HashMap<String, NetworkImpairmentLease>,
    pub network_generations: HashMap<String, u64>,
    pub network_acquire_response_fail_times: u32,
    pub policy_calls: Vec<(String, u64)>,
}

pub struct MockBackend {
    pub min_relay: f64,
    pub world: Mutex<MockWorld>,
}

impl MockBackend {
    pub fn new() -> Self {
        let defaults = live_tuning::staged_map(&BTreeMap::new());
        let mining_policy = MiningTuning::from_source(&defaults).expect("default mining policy");
        let spam_policy = SpamTuning::from_source(&defaults)
            .expect("default spam policy")
            .0;
        Self {
            min_relay: 0.00001,
            world: Mutex::new(MockWorld {
                mining_available: true,
                spam_available: true,
                node1_ok: true,
                mining_state: DesiredState::Running,
                mining_generation: 0,
                mining_policy,
                mining_leases: HashMap::new(),
                spam_state: DesiredState::Running,
                spam_generation: 0,
                spam_policy,
                spam_leases: HashMap::new(),
                mining_policy_fail_times: 0,
                spam_policy_fail_times: 0,
                mining_restore_fail_times: 0,
                spam_restore_fail_times: 0,
                kill_node1_on_policy: false,
                mining_status_fail_times: 0,
                spam_status_fail_times: 0,
                network_leases: HashMap::new(),
                network_generations: HashMap::new(),
                network_acquire_response_fail_times: 0,
                policy_calls: Vec::new(),
            }),
        }
    }

    pub fn sync_workers(&self) {
        let defaults = live_tuning::staged_map(&BTreeMap::new());
        let mut world = self.world.lock().expect("world lock");
        world.mining_available = true;
        world.spam_available = true;
        world.mining_policy = MiningTuning::from_source(&defaults).expect("default mining policy");
        world.spam_policy = SpamTuning::from_source(&defaults)
            .expect("default spam policy")
            .0;
        world.mining_generation = 0;
        world.spam_generation = 0;
    }

    pub fn set_worker_available(&self, component: &str, available: bool) {
        let mut world = self.world.lock().expect("world lock");
        match component {
            MINING_COMPONENT => world.mining_available = available,
            SPAM_COMPONENT => world.spam_available = available,
            _ => panic!("unknown mock worker {component}"),
        }
    }

    pub fn set_worker_policy_value(&self, component: &str, key: &str, value: &str) {
        let mut world = self.world.lock().expect("world lock");
        match component {
            MINING_COMPONENT => {
                let mut source = canonical_source(world.mining_policy.canonical_values());
                source.insert(key.to_string(), value.to_string());
                world.mining_policy = MiningTuning::from_source(&source).expect("mining policy");
            }
            SPAM_COMPONENT => {
                let mut source = canonical_source(world.spam_policy.canonical_values());
                source.insert(key.to_string(), value.to_string());
                world.spam_policy = SpamTuning::from_source(&source).expect("spam policy").0;
            }
            _ => panic!("unknown mock worker {component}"),
        }
    }

    pub fn policy_calls(&self) -> Vec<(String, u64)> {
        self.world.lock().expect("world lock").policy_calls.clone()
    }
}

fn canonical_source(values: BTreeMap<&'static str, String>) -> BTreeMap<String, String> {
    values
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

impl ChainBackend for MockBackend {
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

impl FaucetBackend for MockBackend {
    fn preflight(&self) -> anyhow::Result<FaucetPreflight> {
        let inputs = ["01", "02", "03"]
            .into_iter()
            .map(|byte| FaucetInput {
                txid: byte.repeat(32),
                vout: 0,
                amount_sats: 50_000_000_000,
                confirmations: 204,
            })
            .collect::<Vec<_>>();
        Ok(FaucetPreflight {
            height: 204,
            best_hash: "04".repeat(32),
            node2_inputs: inputs.clone(),
            node3_inputs: inputs,
        })
    }

    fn lock_inputs(
        &self,
        _source: FaucetSourceNode,
        _inputs: &[FaucetInput],
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn unlock_inputs(
        &self,
        _source: FaucetSourceNode,
        _inputs: &[FaucetInput],
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn prepare_transaction(
        &self,
        _source: FaucetSourceNode,
        inputs: &[FaucetInput],
        outputs: &[FaucetOutput],
    ) -> anyhow::Result<PreparedFaucetTransaction> {
        use bitcoincore_rpc::bitcoin::{
            absolute::LockTime, consensus::encode::serialize_hex, transaction::Version, Amount,
            OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
        };
        use std::str::FromStr;
        let input_sats: u64 = inputs.iter().map(|input| input.amount_sats).sum();
        let total_sats: u64 = outputs.iter().map(|output| output.amount_sats).sum();
        let change_sats = input_sats - total_sats;
        let transaction = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_str(&inputs[0].txid)?, inputs[0].vout),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(input_sats),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        Ok(PreparedFaucetTransaction {
            raw_tx_hex: serialize_hex(&transaction),
            txid: transaction.compute_txid().to_string(),
            input_sats,
            change_sats,
            vsize: transaction.vsize() as u64,
        })
    }

    fn set_priority(
        &self,
        _node: FaucetSourceNode,
        _txid: &str,
        desired_delta_sats: i64,
    ) -> anyhow::Result<PriorityUpdate> {
        Ok(PriorityUpdate {
            previous_delta_sats: 0,
            desired_delta_sats,
        })
    }

    fn test_accept(&self, _node: FaucetSourceNode, _raw_tx_hex: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn submit(
        &self,
        _node: FaucetSourceNode,
        _raw_tx_hex: &str,
        _txid: &str,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn verify_miner(
        &self,
        _node: FaucetSourceNode,
        _txid: &str,
    ) -> anyhow::Result<MinerVerification> {
        Ok(MinerVerification {
            base_fee_sats: 0,
            modified_fee_sats: FAUCET_PRIORITY_DELTA_SATS as u64,
            fee_delta_sats: FAUCET_PRIORITY_DELTA_SATS,
            vsize: 60,
            weight: Some(240),
            ancestor_count: 1,
            greatest_competing_feerate_sat_vb: 10,
            minimum_feerate_sat_vb: 1,
        })
    }

    fn observer_contains_unconfirmed(&self, _txid: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn confirmation(&self, _txid: &str) -> anyhow::Result<Option<FaucetConfirmation>> {
        Ok(None)
    }

    fn inputs_unspent(
        &self,
        _source: FaucetSourceNode,
        _inputs: &[FaucetInput],
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
}

impl ScenarioActionBackend for MockBackend {
    fn wait_height(
        &self,
        height: u64,
        control: &dyn simchain_scenario_engine::ScenarioControl,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "target_height": height,
            "final_height": height,
            "aborted": control.abort_requested()
        }))
    }

    fn mine(
        &self,
        node: simchain_scenario_engine::MinerNode,
        blocks: u64,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "node": node.to_string(),
            "blocks": blocks,
            "last_hash": format!("{}-{blocks}", node.short_name())
        }))
    }

    fn spam_burst(
        &self,
        node: simchain_scenario_engine::MinerNode,
        txs: u64,
        outputs_per_tx: u64,
        control: &dyn simchain_scenario_engine::ScenarioControl,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "node": node.to_string(),
            "accepted_transactions": txs,
            "outputs_per_transaction": outputs_per_tx,
            "aborted": control.abort_requested()
        }))
    }

    fn live_summary(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "height": 204,
            "best_block_hash": "mock",
            "mining": MiningControlBackend::status(self)?,
            "spam": SpamControlBackend::status(self)?
        }))
    }
}

impl MiningControlBackend for MockBackend {
    fn status(&self) -> anyhow::Result<MiningWorkerStatus> {
        let mut world = self.world.lock().expect("world lock");
        if world.mining_status_fail_times > 0 {
            world.mining_status_fail_times -= 1;
            anyhow::bail!("simulated mining status failure");
        }
        anyhow::ensure!(world.mining_available, "mining worker unavailable");
        let paused = world.mining_state == DesiredState::Paused || !world.mining_leases.is_empty();
        Ok(MiningWorkerStatus {
            component: MINING_COMPONENT.to_string(),
            phase: if paused {
                WorkerPhase::Paused
            } else {
                WorkerPhase::Running
            },
            desired_state: world.mining_state,
            effective_state: if paused {
                DesiredState::Paused
            } else {
                DesiredState::Running
            },
            effective_rng_seed: world.mining_policy.rng_seed.unwrap_or(1),
            policy: world.mining_policy.clone(),
            effective_generation: world.mining_generation,
            height: Some(100),
            next_scheduled_attempt_ms: None,
            last_mined_block: None,
            active_leases: world.mining_leases.values().cloned().collect(),
            uptime_secs: 1,
            last_error: None,
        })
    }

    fn set_state(&self, state: DesiredState) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        anyhow::ensure!(world.mining_available, "mining worker unavailable");
        world.mining_state = state;
        Ok(CommandAck {
            request_id: "mock-mining-state".to_string(),
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
        anyhow::ensure!(world.mining_available, "mining worker unavailable");
        if world.mining_policy_fail_times > 0 {
            world.mining_policy_fail_times -= 1;
            anyhow::bail!("simulated mining policy failure");
        }
        world.mining_policy = policy;
        world.mining_generation = generation;
        world
            .policy_calls
            .push((MINING_COMPONENT.to_string(), generation));
        Ok(CommandAck {
            request_id: "mock-mining-policy".to_string(),
            phase: WorkerPhase::Running,
            effective_generation: generation,
        })
    }

    fn restore_policy(&self, generation: u64, policy: MiningTuning) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        anyhow::ensure!(world.mining_available, "mining worker unavailable");
        if world.mining_restore_fail_times > 0 {
            world.mining_restore_fail_times -= 1;
            anyhow::bail!("simulated mining restore failure");
        }
        world.mining_policy = policy;
        world.mining_generation = generation;
        Ok(CommandAck {
            request_id: "mock-mining-restore".to_string(),
            phase: WorkerPhase::Running,
            effective_generation: generation,
        })
    }

    fn acquire_lease(&self, request: LeaseRequest) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        anyhow::ensure!(world.mining_available, "mining worker unavailable");
        world.mining_leases.insert(
            request.lease_id.clone(),
            PauseLease {
                lease_id: request.lease_id,
                owner_job_id: request.owner_job_id,
                purpose: request.purpose,
                expires_at_ms: u64::MAX,
            },
        );
        Ok(CommandAck {
            request_id: request.request_id,
            phase: WorkerPhase::Paused,
            effective_generation: world.mining_generation,
        })
    }

    fn renew_lease(
        &self,
        lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<CommandAck> {
        let world = self.world.lock().expect("world lock");
        anyhow::ensure!(
            world.mining_leases.contains_key(lease_id),
            "lease not found"
        );
        Ok(CommandAck {
            request_id: request.request_id,
            phase: WorkerPhase::Paused,
            effective_generation: world.mining_generation,
        })
    }

    fn release_lease(
        &self,
        lease_id: &str,
        request: LeaseReleaseRequest,
    ) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        world.mining_leases.remove(lease_id);
        let paused = world.mining_state == DesiredState::Paused || !world.mining_leases.is_empty();
        Ok(CommandAck {
            request_id: request.request_id,
            phase: if paused {
                WorkerPhase::Paused
            } else {
                WorkerPhase::Running
            },
            effective_generation: world.mining_generation,
        })
    }
}

impl SpamControlBackend for MockBackend {
    fn status(&self) -> anyhow::Result<SpamWorkerStatus> {
        let mut world = self.world.lock().expect("world lock");
        if world.spam_status_fail_times > 0 {
            world.spam_status_fail_times -= 1;
            anyhow::bail!("simulated spam status failure");
        }
        anyhow::ensure!(world.spam_available, "spam worker unavailable");
        let paused = world.spam_state == DesiredState::Paused || !world.spam_leases.is_empty();
        let phase = if paused {
            WorkerPhase::Paused
        } else if world.spam_policy.enabled {
            WorkerPhase::Active
        } else {
            WorkerPhase::Disabled
        };
        Ok(SpamWorkerStatus {
            component: SPAM_COMPONENT.to_string(),
            phase,
            desired_state: world.spam_state,
            effective_state: if paused {
                DesiredState::Paused
            } else {
                DesiredState::Running
            },
            policy: world.spam_policy.clone(),
            effective_generation: world.spam_generation,
            observed_height: Some(100),
            cycle_phase: Some("waiting_for_block".to_string()),
            accepted_transactions: 10,
            last_cycle_duration_ms: Some(1_250),
            active_leases: world.spam_leases.values().cloned().collect(),
            reconciliation_pending: false,
            uptime_secs: 1,
            last_error: None,
        })
    }

    fn set_state(&self, state: DesiredState) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        anyhow::ensure!(world.spam_available, "spam worker unavailable");
        world.spam_state = state;
        Ok(CommandAck {
            request_id: "mock-spam-state".to_string(),
            phase: if state == DesiredState::Paused {
                WorkerPhase::Paused
            } else if world.spam_policy.enabled {
                WorkerPhase::Active
            } else {
                WorkerPhase::Disabled
            },
            effective_generation: world.spam_generation,
        })
    }

    fn set_policy(&self, generation: u64, policy: SpamTuning) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        anyhow::ensure!(world.spam_available, "spam worker unavailable");
        if world.spam_policy_fail_times > 0 {
            world.spam_policy_fail_times -= 1;
            anyhow::bail!("simulated spam policy failure");
        }
        if world.kill_node1_on_policy {
            world.node1_ok = false;
        }
        let enabled = policy.enabled;
        world.spam_policy = policy;
        world.spam_generation = generation;
        world
            .policy_calls
            .push((SPAM_COMPONENT.to_string(), generation));
        Ok(CommandAck {
            request_id: "mock-spam-policy".to_string(),
            phase: if enabled {
                WorkerPhase::Active
            } else {
                WorkerPhase::Disabled
            },
            effective_generation: generation,
        })
    }

    fn restore_policy(&self, generation: u64, policy: SpamTuning) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        anyhow::ensure!(world.spam_available, "spam worker unavailable");
        if world.spam_restore_fail_times > 0 {
            world.spam_restore_fail_times -= 1;
            anyhow::bail!("simulated spam restore failure");
        }
        let enabled = policy.enabled;
        world.spam_policy = policy;
        world.spam_generation = generation;
        Ok(CommandAck {
            request_id: "mock-spam-restore".to_string(),
            phase: if enabled {
                WorkerPhase::Active
            } else {
                WorkerPhase::Disabled
            },
            effective_generation: generation,
        })
    }

    fn acquire_lease(&self, request: LeaseRequest) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        anyhow::ensure!(world.spam_available, "spam worker unavailable");
        world.spam_leases.insert(
            request.lease_id.clone(),
            PauseLease {
                lease_id: request.lease_id,
                owner_job_id: request.owner_job_id,
                purpose: request.purpose,
                expires_at_ms: u64::MAX,
            },
        );
        Ok(CommandAck {
            request_id: request.request_id,
            phase: WorkerPhase::Paused,
            effective_generation: world.spam_generation,
        })
    }

    fn renew_lease(
        &self,
        lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<CommandAck> {
        let world = self.world.lock().expect("world lock");
        anyhow::ensure!(world.spam_leases.contains_key(lease_id), "lease not found");
        Ok(CommandAck {
            request_id: request.request_id,
            phase: WorkerPhase::Paused,
            effective_generation: world.spam_generation,
        })
    }

    fn release_lease(
        &self,
        lease_id: &str,
        request: LeaseReleaseRequest,
    ) -> anyhow::Result<CommandAck> {
        let mut world = self.world.lock().expect("world lock");
        world.spam_leases.remove(lease_id);
        let paused = world.spam_state == DesiredState::Paused || !world.spam_leases.is_empty();
        Ok(CommandAck {
            request_id: request.request_id,
            phase: if paused {
                WorkerPhase::Paused
            } else if world.spam_policy.enabled {
                WorkerPhase::Active
            } else {
                WorkerPhase::Disabled
            },
            effective_generation: world.spam_generation,
        })
    }
}

impl NetworkControlBackend for MockBackend {
    fn status(&self, node: &str) -> anyhow::Result<NetworkAgentStatus> {
        let node = normalize_mock_node(node)?;
        let world = self.world.lock().expect("world lock");
        Ok(NetworkAgentStatus {
            component: "network-agent".to_string(),
            node: node.to_string(),
            p2p_interface: "eth1".to_string(),
            effective_generation: world.network_generations.get(node).copied().unwrap_or(0),
            active_lease: world.network_leases.get(node).cloned(),
            uptime_secs: 1,
            last_error: None,
        })
    }

    fn acquire_lease(
        &self,
        node: &str,
        request: NetworkLeaseRequest,
    ) -> anyhow::Result<NetworkCommandAck> {
        let node = normalize_mock_node(node)?;
        let mut world = self.world.lock().expect("world lock");
        if let Some(active) = world.network_leases.get(node) {
            anyhow::ensure!(
                active.lease_id == request.lease_id,
                "another lease is active"
            );
        } else {
            world.network_leases.insert(
                node.to_string(),
                NetworkImpairmentLease {
                    lease_id: request.lease_id,
                    owner_job_id: request.owner_job_id,
                    purpose: request.purpose,
                    expires_at_ms: u64::MAX,
                    impairment: request.impairment,
                },
            );
            *world
                .network_generations
                .entry(node.to_string())
                .or_default() += 1;
        }
        if world.network_acquire_response_fail_times > 0 {
            world.network_acquire_response_fail_times -= 1;
            anyhow::bail!("simulated lost network lease acquisition response");
        }
        Ok(NetworkCommandAck {
            request_id: request.request_id,
            effective_generation: world.network_generations.get(node).copied().unwrap_or(0),
            impairment_active: true,
        })
    }

    fn renew_lease(
        &self,
        node: &str,
        lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<NetworkCommandAck> {
        let status = NetworkControlBackend::status(self, node)?;
        anyhow::ensure!(
            status
                .active_lease
                .as_ref()
                .is_some_and(|lease| lease.lease_id == lease_id),
            "lease not found"
        );
        Ok(NetworkCommandAck {
            request_id: request.request_id,
            effective_generation: status.effective_generation,
            impairment_active: true,
        })
    }

    fn release_lease(
        &self,
        node: &str,
        lease_id: &str,
        request: NetworkLeaseReleaseRequest,
    ) -> anyhow::Result<NetworkCommandAck> {
        let node = normalize_mock_node(node)?;
        let mut world = self.world.lock().expect("world lock");
        if let Some(active) = world.network_leases.get(node) {
            anyhow::ensure!(active.lease_id == lease_id, "different lease is active");
            world.network_leases.remove(node);
            *world
                .network_generations
                .entry(node.to_string())
                .or_default() += 1;
        }
        Ok(NetworkCommandAck {
            request_id: request.request_id,
            effective_generation: world.network_generations.get(node).copied().unwrap_or(0),
            impairment_active: false,
        })
    }
}

impl NetworkActionBackend for MockBackend {
    fn validate_ready_and_converged(&self) -> anyhow::Result<ChainSnapshot> {
        Ok(mock_snapshot("mock"))
    }

    fn disconnect_target_peers(
        &self,
        _node: simchain_scenario_engine::MinerNode,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn wait_for_isolation(
        &self,
        node: simchain_scenario_engine::MinerNode,
        control: &dyn simchain_scenario_engine::ScenarioControl,
    ) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!(!control.abort_requested(), "aborted");
        Ok(serde_json::json!({"isolated_node": node.short_name()}))
    }

    fn reconnect_target(&self, _node: simchain_scenario_engine::MinerNode) -> anyhow::Result<()> {
        Ok(())
    }

    fn wait_for_convergence(
        &self,
        expected_hash: Option<&str>,
        _control: &dyn simchain_scenario_engine::ScenarioControl,
    ) -> anyhow::Result<ChainSnapshot> {
        Ok(mock_snapshot(expected_hash.unwrap_or("mock")))
    }
}

fn normalize_mock_node(node: &str) -> anyhow::Result<&str> {
    match node {
        "node1" | "btc-simnet-node1" => Ok("node1"),
        "node2" | "btc-simnet-node2" => Ok("node2"),
        "node3" | "btc-simnet-node3" => Ok("node3"),
        _ => anyhow::bail!("invalid mock node"),
    }
}

fn mock_snapshot(hash: &str) -> ChainSnapshot {
    ChainSnapshot {
        height: 204,
        best_hash: hash.to_string(),
        tips: BTreeMap::from([
            ("node1".to_string(), hash.to_string()),
            ("node2".to_string(), hash.to_string()),
            ("node3".to_string(), hash.to_string()),
        ]),
    }
}

pub fn test_app(dir: &Path, backend: Arc<MockBackend>) -> AppState {
    let state_dir = dir.join(".simchain-control");
    let store = ControlStateStore::open(state_dir.clone()).expect("control store");
    let instance_guard = store
        .try_instance_lock()
        .expect("instance lock")
        .expect("single test control plane");
    let control_state = store
        .load_or_initialize(ControlState::default().desired)
        .expect("control state");
    let jobs = JobManager::open(
        &state_dir,
        JobDependencies {
            mining: backend.clone(),
            spam: backend.clone(),
            network: backend.clone(),
            reorg: Arc::new(MockReorgExecutor),
            scenario: backend.clone(),
            network_actions: backend.clone(),
            faucet: backend.clone(),
            faucet_settings: FaucetSettings {
                node2_wallet_name: "node2".to_string(),
                node3_wallet_name: "node3".to_string(),
                wallet_reserve_sats: 60_000_000_000,
                max_request_sats: 10_000_000_000,
                explorer_url: "http://127.0.0.1:1080".to_string(),
            },
        },
    )
    .expect("job manager");
    AppState {
        config: ControlPlaneConfig {
            listen_addr: "127.0.0.1:0".parse().expect("addr"),
            node1_url: "http://mock-node1:18443".to_string(),
            node2_url: "http://mock-node2:18443".to_string(),
            node3_url: "http://mock-node3:18443".to_string(),
            state_dir,
            mining_control_url: "http://mock-mining:9081".to_string(),
            spam_control_url: "http://mock-spam:9082".to_string(),
            node1_network_agent_url: "http://mock-node1:9083".to_string(),
            node2_network_agent_url: "http://mock-node2:9083".to_string(),
            node3_network_agent_url: "http://mock-node3:9083".to_string(),
            internal_token: "test-internal-token".to_string(),
            explorer_url: "http://127.0.0.1:1080".to_string(),
            explorer_probe_url: "http://mempool-web:8080".to_string(),
            node2_wallet_name: "node2".to_string(),
            node3_wallet_name: "node3".to_string(),
            faucet_wallet_reserve_sats: 60_000_000_000,
            faucet_max_request_sats: 10_000_000_000,
        },
        token: "test-token".to_string(),
        chain: backend.clone(),
        mining: backend.clone(),
        spam: backend.clone(),
        network: backend,
        jobs,
        control_state: RwLock::new(control_state),
        control_store: store,
        status: RwLock::new(StatusSnapshot::default()),
        _instance_guard: instance_guard,
        apply_lock: Mutex::new(()),
    }
}
