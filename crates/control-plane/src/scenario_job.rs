//! Production action adapter for the pure scenario engine. Chain and wallet
//! actions use Bitcoin RPC; partition coordination belongs to the job manager
//! and namespace-local network agents.

use crate::backend::{MiningControlBackend, SpamControlBackend};
use crate::state::ControlPlaneConfig;
use anyhow::{Context, Result};
use bitcoincore_rpc::bitcoin::Txid;
use bitcoincore_rpc::RpcApi;
use serde_json::{json, Value};
use simchain_common::config::{
    parse_rpc_url, RpcUrl, DEFAULT_NODE2_WALLET_NAME, DEFAULT_NODE3_WALLET_NAME,
};
use simchain_common::{
    create_client, create_jsonrpc_client, create_wallet_client, require_regtest_address,
};
use simchain_scenario_engine::{MinerNode, ScenarioControl, TxWaitState};
use simchain_spammer::raw_transaction_spammer::RawSpammer;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_SCENARIO_TIMEOUT_SECS: u64 = 1_800;

pub trait ScenarioActionBackend: Send + Sync {
    fn wait_height(&self, height: u64, control: &dyn ScenarioControl) -> Result<Value>;
    fn mine(&self, node: MinerNode, blocks: u64) -> Result<Value>;
    /// Fund the raw burst engines for `nodes` so later `spam_burst` steps can
    /// run without block production (funding needs confirmations, bursts do
    /// not). Called before the scenario's steps, while mining still runs.
    fn prepare_spam_burst(
        &self,
        nodes: &[MinerNode],
        control: &dyn ScenarioControl,
    ) -> Result<Value>;
    fn spam_burst(
        &self,
        node: MinerNode,
        txs: u64,
        outputs_per_tx: u64,
        control: &dyn ScenarioControl,
    ) -> Result<Value>;
    fn wait_tx(
        &self,
        txid: &str,
        state: TxWaitState,
        confirmations: u64,
        timeout_secs: u64,
        control: &dyn ScenarioControl,
    ) -> Result<Value>;
    fn live_summary(&self) -> Result<Value>;
}

/// One cached raw burst engine per miner node. Kept across bursts so
/// consecutive bursts chain correctly off in-memory unconfirmed change.
#[derive(Default)]
struct BurstEngines {
    node2: Option<RawSpammer>,
    node3: Option<RawSpammer>,
}

pub struct RpcScenarioActionBackend {
    node1_url: RpcUrl,
    node2_url: RpcUrl,
    node3_url: RpcUrl,
    node2_wallet: String,
    node3_wallet: String,
    timeout: Duration,
    mining: Arc<dyn MiningControlBackend>,
    spam: Arc<dyn SpamControlBackend>,
    burst_engines: Mutex<BurstEngines>,
}

impl RpcScenarioActionBackend {
    pub fn from_config(
        config: &ControlPlaneConfig,
        mining: Arc<dyn MiningControlBackend>,
        spam: Arc<dyn SpamControlBackend>,
    ) -> Result<Self> {
        let timeout_secs = std::env::var("SCENARIO_TIMEOUT_SECS")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.parse::<u64>())
            .transpose()
            .context("SCENARIO_TIMEOUT_SECS must be a positive integer")?
            .unwrap_or(DEFAULT_SCENARIO_TIMEOUT_SECS);
        anyhow::ensure!(timeout_secs > 0, "SCENARIO_TIMEOUT_SECS must be positive");
        Ok(Self {
            node1_url: parse_rpc_url("NODE1_RPC_URL", config.node1_url.clone())?,
            node2_url: parse_rpc_url("NODE2_RPC_URL", config.node2_url.clone())?,
            node3_url: parse_rpc_url("NODE3_RPC_URL", config.node3_url.clone())?,
            node2_wallet: non_empty_env("NODE2_WALLET_NAME", DEFAULT_NODE2_WALLET_NAME),
            node3_wallet: non_empty_env("NODE3_WALLET_NAME", DEFAULT_NODE3_WALLET_NAME),
            timeout: Duration::from_secs(timeout_secs),
            mining,
            spam,
            burst_engines: Mutex::new(BurstEngines::default()),
        })
    }

    fn target(&self, node: MinerNode) -> (&RpcUrl, &str) {
        match node {
            MinerNode::Node2 => (&self.node2_url, &self.node2_wallet),
            MinerNode::Node3 => (&self.node3_url, &self.node3_wallet),
        }
    }

    /// Scenario bursts run on a dedicated raw engine per miner node: locally
    /// signed transactions submitted with sendrawtransaction, so no coin
    /// selection or signing load lands on the miner nodes' wallets (the
    /// engine pulls one wallet funding transaction only when broke). The key
    /// namespace is scenario-specific so these instances never track — and
    /// double-spend — the resident spammer's UTXO set.
    fn with_burst_engine<R>(
        &self,
        node: MinerNode,
        fee_rate_sat_vb: f64,
        action: impl FnOnce(&mut RawSpammer) -> Result<R>,
    ) -> Result<R> {
        let mut engines = self.burst_engines.lock().expect("burst engine lock");
        let slot = match node {
            MinerNode::Node2 => &mut engines.node2,
            MinerNode::Node3 => &mut engines.node3,
        };
        if slot.is_none() {
            let (rpc_url, wallet_name) = self.target(node);
            let mut engine = RawSpammer::new(
                create_client(rpc_url)?,
                create_jsonrpc_client(rpc_url)?,
                Vec::new(),
                create_wallet_client(rpc_url, wallet_name)?,
                wallet_name,
                &format!("scenario-{wallet_name}"),
                &format!("Scenario burst {}", node.short_name()),
                fee_rate_sat_vb,
                0,
                0,
                0,
            );
            engine
                .reconcile()
                .with_context(|| format!("reconcile the scenario burst engine for {node}"))?;
            *slot = Some(engine);
        }
        action(slot.as_mut().expect("burst engine present"))
    }

    /// The live spam policy supplies the burst fee rate and branch sizing so
    /// scenario traffic prices like resident spam.
    fn burst_policy(&self) -> Result<simchain_common::live_tuning::SpamTuning> {
        Ok(self
            .spam
            .status()
            .context("read the live spam policy for scenario bursts")?
            .policy)
    }
}

impl ScenarioActionBackend for RpcScenarioActionBackend {
    fn wait_height(&self, target: u64, control: &dyn ScenarioControl) -> Result<Value> {
        let client = create_client(&self.node1_url)?;
        let started = Instant::now();
        let mut initial = None;
        loop {
            match client.get_block_count() {
                Ok(height) => {
                    initial.get_or_insert(height);
                    if height >= target || control.abort_requested() {
                        return Ok(json!({
                            "initial_height": initial,
                            "final_height": height,
                            "target_height": target,
                            "aborted": control.abort_requested()
                        }));
                    }
                }
                Err(error) if started.elapsed() >= self.timeout => {
                    return Err(anyhow::anyhow!(error))
                        .with_context(|| format!("timed out waiting for node1 height {target}"));
                }
                Err(_) => {}
            }
            if started.elapsed() >= self.timeout {
                anyhow::bail!("timed out waiting for node1 height {target}");
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    fn mine(&self, node: MinerNode, blocks: u64) -> Result<Value> {
        let (rpc_url, wallet_name) = self.target(node);
        let wallet = create_wallet_client(rpc_url, wallet_name)?;
        let client = create_client(rpc_url)?;
        let address = require_regtest_address(
            wallet
                .get_new_address(None, None)
                .context("get fresh mining address")?,
        )?;
        let hashes = client
            .generate_to_address(blocks, &address)
            .with_context(|| format!("mine {blocks} blocks on {node}"))?;
        Ok(json!({
            "node": node.to_string(),
            "blocks": blocks,
            "first_hash": hashes.first().map(ToString::to_string),
            "last_hash": hashes.last().map(ToString::to_string)
        }))
    }

    fn prepare_spam_burst(
        &self,
        nodes: &[MinerNode],
        control: &dyn ScenarioControl,
    ) -> Result<Value> {
        let policy = self.burst_policy()?;
        let branches = policy.fanout_utxos.max(1);
        let mut prepared = Vec::new();
        for node in nodes {
            self.with_burst_engine(*node, policy.fee_rate_sat_vb(), |engine| {
                engine.set_burst_shape(policy.fee_rate_sat_vb(), 0);
                let checkpoint = |_: &str| !control.abort_requested();
                anyhow::ensure!(
                    engine.ensure_branches(branches, &checkpoint),
                    "interrupted while funding the scenario burst engine for {node}"
                );
                Ok(())
            })?;
            prepared.push(node.to_string());
        }
        Ok(json!({"prepared": prepared, "branches": branches}))
    }

    fn spam_burst(
        &self,
        node: MinerNode,
        txs: u64,
        outputs_per_tx: u64,
        control: &dyn ScenarioControl,
    ) -> Result<Value> {
        let policy = self.burst_policy()?;
        let fanout = policy.fanout_utxos.max(1);
        self.with_burst_engine(node, policy.fee_rate_sat_vb(), |engine| {
            engine.set_burst_shape(policy.fee_rate_sat_vb(), outputs_per_tx);
            let checkpoint = |_: &str| !control.abort_requested();
            let mut txids = engine.output_round(txs, fanout, false, 0, &checkpoint);
            if (txids.len() as u64) < txs && !control.abort_requested() {
                // A chain mutation between steps (reorg, partition) may have
                // invalidated in-memory branches; resync once and finish the
                // remainder.
                engine
                    .reconcile()
                    .context("reconcile the scenario burst engine mid-burst")?;
                let remaining = txs - txids.len() as u64;
                txids.extend(engine.output_round(remaining, fanout, false, 0, &checkpoint));
            }
            Ok(json!({
                "node": node.to_string(),
                "requested_transactions": txs,
                "accepted_transactions": txids.len() as u64,
                "outputs_per_transaction": outputs_per_tx,
                "engine": "raw",
                "aborted": control.abort_requested()
            }))
        })
    }

    fn wait_tx(
        &self,
        txid: &str,
        state: TxWaitState,
        confirmations: u64,
        timeout_secs: u64,
        control: &dyn ScenarioControl,
    ) -> Result<Value> {
        let txid = Txid::from_str(txid).context("invalid txid")?;
        let client = create_client(&self.node1_url)?;
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let started = Instant::now();
        loop {
            let observed = observe_tx(&client, &txid)?;
            if tx_wait_satisfied(&observed, state, confirmations) || control.abort_requested() {
                let mut result = json!({
                    "txid": txid.to_string(),
                    "target_state": state.as_str(),
                    "timeout_secs": timeout_secs,
                    "observation": observed,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                    "aborted": control.abort_requested()
                });
                if state == TxWaitState::Confirmed {
                    result["target_confirmations"] = json!(confirmations);
                }
                return Ok(result);
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out after {timeout_secs}s waiting for tx {} to become {}{}; last observed: {}",
                    txid,
                    state.as_str(),
                    if state == TxWaitState::Confirmed {
                        format!(" with at least {confirmations} confirmation(s)")
                    } else {
                        String::new()
                    },
                    observed
                );
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    fn live_summary(&self) -> Result<Value> {
        let node = create_client(&self.node1_url)?;
        let mempool = node.get_mempool_info()?;
        Ok(json!({
            "height": node.get_block_count()?,
            "best_block_hash": node.get_best_block_hash()?.to_string(),
            "mempool": {
                "transactions": mempool.size,
                "bytes": mempool.bytes,
                "usage": mempool.usage
            },
            "mining": self.mining.status()?,
            "spam": self.spam.status()?
        }))
    }
}

fn observe_tx(client: &bitcoincore_rpc::Client, txid: &Txid) -> Result<Value> {
    let info = match client.get_raw_transaction_info(txid, None) {
        Ok(info) => info,
        Err(bitcoincore_rpc::Error::JsonRpc(bitcoincore_rpc::jsonrpc::error::Error::Rpc(
            error,
        ))) if error.code == -5 => {
            return Ok(json!({
                "state": TxWaitState::Missing.as_str(),
                "in_mempool": false,
                "confirmations": 0
            }));
        }
        Err(error) => return Err(error.into()),
    };
    let confirmations = info.confirmations.unwrap_or(0);
    let Some(block_hash) = info.blockhash else {
        return Ok(json!({
            "state": TxWaitState::Mempool.as_str(),
            "in_mempool": true,
            "confirmations": confirmations
        }));
    };
    if confirmations == 0 {
        return Ok(json!({
            "state": TxWaitState::Seen.as_str(),
            "in_mempool": false,
            "confirmations": 0,
            "block_hash": block_hash.to_string()
        }));
    }
    let header = client.get_block_header_info(&block_hash)?;
    Ok(json!({
        "state": TxWaitState::Confirmed.as_str(),
        "in_mempool": false,
        "confirmations": confirmations,
        "block_hash": block_hash.to_string(),
        "height": header.height
    }))
}

fn tx_wait_satisfied(observed: &Value, state: TxWaitState, confirmations: u64) -> bool {
    let observed_state = observed.get("state").and_then(Value::as_str);
    match state {
        TxWaitState::Seen => {
            observed_state.is_some_and(|state| state != TxWaitState::Missing.as_str())
        }
        TxWaitState::Mempool => observed_state == Some(TxWaitState::Mempool.as_str()),
        TxWaitState::Missing => observed_state == Some(TxWaitState::Missing.as_str()),
        TxWaitState::Confirmed => {
            observed_state == Some(TxWaitState::Confirmed.as_str())
                && observed
                    .get("confirmations")
                    .and_then(Value::as_u64)
                    .is_some_and(|actual| actual >= confirmations)
        }
    }
}

fn non_empty_env(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_wait_state_matching_is_explicit() {
        let missing = json!({"state": "missing", "confirmations": 0});
        let mempool = json!({"state": "mempool", "confirmations": 0});
        let confirmed_one = json!({"state": "confirmed", "confirmations": 1});
        let confirmed_two = json!({"state": "confirmed", "confirmations": 2});

        assert!(tx_wait_satisfied(&mempool, TxWaitState::Seen, 0));
        assert!(tx_wait_satisfied(&confirmed_one, TxWaitState::Seen, 0));
        assert!(!tx_wait_satisfied(&missing, TxWaitState::Seen, 0));

        assert!(tx_wait_satisfied(&mempool, TxWaitState::Mempool, 0));
        assert!(!tx_wait_satisfied(&confirmed_one, TxWaitState::Mempool, 0));

        assert!(tx_wait_satisfied(&confirmed_two, TxWaitState::Confirmed, 2));
        assert!(!tx_wait_satisfied(
            &confirmed_one,
            TxWaitState::Confirmed,
            2
        ));
        assert!(tx_wait_satisfied(&missing, TxWaitState::Missing, 0));
    }
}
