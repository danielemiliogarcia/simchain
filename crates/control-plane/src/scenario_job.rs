//! Production action adapter for the pure scenario engine. Chain and wallet
//! actions use Bitcoin RPC; only the Phase-6 partition placeholder delegates
//! to the control plane's existing transitional adapter.

use crate::backend::{JobActions, MiningControlBackend, SpamControlBackend};
use crate::state::ControlPlaneConfig;
use anyhow::{Context, Result};
use bitcoincore_rpc::bitcoin::Amount;
use bitcoincore_rpc::RpcApi;
use serde_json::{json, Value};
use simchain_common::config::{
    parse_rpc_url, RpcUrl, DEFAULT_NODE2_WALLET_NAME, DEFAULT_NODE3_WALLET_NAME,
};
use simchain_common::{burn_address, create_client, create_wallet_client, require_regtest_address};
use simchain_scenario_engine::{MinerNode, ScenarioControl};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const DUST_SATS: u64 = 546;
const DEFAULT_SCENARIO_TIMEOUT_SECS: u64 = 1_800;

pub trait ScenarioActionBackend: Send + Sync {
    fn wait_height(&self, height: u64, control: &dyn ScenarioControl) -> Result<Value>;
    fn mine(&self, node: MinerNode, blocks: u64) -> Result<Value>;
    fn spam_burst(
        &self,
        node: MinerNode,
        txs: u64,
        outputs_per_tx: u64,
        control: &dyn ScenarioControl,
    ) -> Result<Value>;
    fn run_partition(
        &self,
        node: MinerNode,
        main_blocks: u64,
        isolated_blocks: u64,
        control: &dyn ScenarioControl,
    ) -> Result<Value>;
    fn live_summary(&self) -> Result<Value>;
}

pub struct RpcScenarioActionBackend {
    node1_url: RpcUrl,
    node2_url: RpcUrl,
    node3_url: RpcUrl,
    node2_wallet: String,
    node3_wallet: String,
    timeout: Duration,
    transitional: Arc<dyn JobActions>,
    mining: Arc<dyn MiningControlBackend>,
    spam: Arc<dyn SpamControlBackend>,
}

impl RpcScenarioActionBackend {
    pub fn from_config(
        config: &ControlPlaneConfig,
        transitional: Arc<dyn JobActions>,
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
            transitional,
            mining,
            spam,
        })
    }

    fn target(&self, node: MinerNode) -> (&RpcUrl, &str) {
        match node {
            MinerNode::Node2 => (&self.node2_url, &self.node2_wallet),
            MinerNode::Node3 => (&self.node3_url, &self.node3_wallet),
        }
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

    fn spam_burst(
        &self,
        node: MinerNode,
        txs: u64,
        outputs_per_tx: u64,
        control: &dyn ScenarioControl,
    ) -> Result<Value> {
        let (rpc_url, wallet_name) = self.target(node);
        let wallet = create_wallet_client(rpc_url, wallet_name)?;
        let mut accepted = 0u64;
        if outputs_per_tx == 0 {
            let address = burn_address(0);
            for number in 1..=txs {
                if control.abort_requested() {
                    break;
                }
                wallet
                    .send_to_address(
                        &address,
                        Amount::from_sat(DUST_SATS),
                        None,
                        None,
                        None,
                        Some(false),
                        None,
                        None,
                    )
                    .with_context(|| format!("spam transaction {number}/{txs} failed"))?;
                accepted += 1;
            }
        } else {
            let mut amounts = serde_json::Map::new();
            for index in 1..=outputs_per_tx {
                amounts.insert(burn_address(index).to_string(), json!("0.00000546"));
            }
            let params = [
                json!(""),
                json!(amounts),
                json!(0),
                json!("scenario spam burst"),
                json!([]),
                json!(false),
            ];
            for number in 1..=txs {
                if control.abort_requested() {
                    break;
                }
                wallet
                    .call::<String>("sendmany", &params)
                    .with_context(|| format!("spam batch {number}/{txs} failed"))?;
                accepted += 1;
            }
        }
        Ok(json!({
            "node": node.to_string(),
            "requested_transactions": txs,
            "accepted_transactions": accepted,
            "outputs_per_transaction": outputs_per_tx,
            "aborted": control.abort_requested()
        }))
    }

    fn run_partition(
        &self,
        node: MinerNode,
        main_blocks: u64,
        isolated_blocks: u64,
        _control: &dyn ScenarioControl,
    ) -> Result<Value> {
        self.transitional
            .run_partition(&node.to_string(), main_blocks, isolated_blocks)?;
        Ok(json!({
            "node": node.to_string(),
            "main_blocks": main_blocks,
            "isolated_blocks": isolated_blocks,
            "adapter": "transitional_control_plane"
        }))
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

fn non_empty_env(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}
