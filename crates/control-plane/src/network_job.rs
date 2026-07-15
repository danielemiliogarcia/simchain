//! Bitcoin-RPC side of deterministic partition coordination. Network
//! impairment itself belongs exclusively to namespace-local agents.

use crate::state::ControlPlaneConfig;
use anyhow::Context;
use bitcoincore_rpc::RpcApi;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use simchain_common::config::{parse_rpc_url, RpcUrl};
use simchain_common::create_client;
use simchain_scenario_engine::{MinerNode, ScenarioControl};
use std::collections::BTreeMap;
use std::thread;
use std::time::{Duration, Instant};

const BOOTSTRAP_HEIGHT: u64 = 204;
const DEFAULT_PEER_TIMEOUT_SECS: u64 = 15;
const DEFAULT_CONVERGENCE_TIMEOUT_SECS: u64 = 60;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChainSnapshot {
    pub height: u64,
    pub best_hash: String,
    pub tips: BTreeMap<String, String>,
}

pub trait NetworkActionBackend: Send + Sync {
    fn validate_ready_and_converged(&self) -> anyhow::Result<ChainSnapshot>;
    fn disconnect_target_peers(&self, node: MinerNode) -> anyhow::Result<()>;
    fn wait_for_isolation(
        &self,
        node: MinerNode,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value>;
    fn reconnect_target(&self, node: MinerNode) -> anyhow::Result<()>;
    fn wait_for_convergence(
        &self,
        expected_hash: Option<&str>,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<ChainSnapshot>;
}

pub struct RpcNetworkActionBackend {
    node1_url: RpcUrl,
    node2_url: RpcUrl,
    node3_url: RpcUrl,
    peer_timeout: Duration,
    convergence_timeout: Duration,
}

impl RpcNetworkActionBackend {
    pub fn from_config(config: &ControlPlaneConfig) -> anyhow::Result<Self> {
        Ok(Self {
            node1_url: parse_rpc_url("NODE1_RPC_URL", config.node1_url.clone())?,
            node2_url: parse_rpc_url("NODE2_RPC_URL", config.node2_url.clone())?,
            node3_url: parse_rpc_url("NODE3_RPC_URL", config.node3_url.clone())?,
            peer_timeout: duration_env("PARTITION_PEER_TIMEOUT_SECS", DEFAULT_PEER_TIMEOUT_SECS)?,
            convergence_timeout: duration_env(
                "PARTITION_CONVERGENCE_TIMEOUT_SECS",
                DEFAULT_CONVERGENCE_TIMEOUT_SECS,
            )?,
        })
    }

    fn url(&self, node: MinerNode) -> &RpcUrl {
        match node {
            MinerNode::Node2 => &self.node2_url,
            MinerNode::Node3 => &self.node3_url,
        }
    }

    fn main_node(node: MinerNode) -> MinerNode {
        match node {
            MinerNode::Node2 => MinerNode::Node3,
            MinerNode::Node3 => MinerNode::Node2,
        }
    }

    fn peer_count(url: &RpcUrl) -> anyhow::Result<usize> {
        let peers: Vec<Value> = create_client(url)?.call("getpeerinfo", &[])?;
        Ok(peers.len())
    }

    fn p2p_peer_count(url: &RpcUrl) -> anyhow::Result<usize> {
        let peers: Vec<Value> = create_client(url)?.call("getpeerinfo", &[])?;
        Ok(peers
            .iter()
            .filter_map(|peer| peer.get("addr").and_then(Value::as_str))
            .filter(|address| is_p2p_address(address))
            .count())
    }

    fn snapshot(&self) -> anyhow::Result<ChainSnapshot> {
        let node1 = create_client(&self.node1_url)?;
        let node2 = create_client(&self.node2_url)?;
        let node3 = create_client(&self.node3_url)?;
        let height = node1.get_block_count()?;
        let best_hash = node1.get_best_block_hash()?.to_string();
        let tips = BTreeMap::from([
            ("node1".to_string(), best_hash.clone()),
            (
                "node2".to_string(),
                node2.get_best_block_hash()?.to_string(),
            ),
            (
                "node3".to_string(),
                node3.get_best_block_hash()?.to_string(),
            ),
        ]);
        Ok(ChainSnapshot {
            height,
            best_hash,
            tips,
        })
    }
}

impl NetworkActionBackend for RpcNetworkActionBackend {
    fn validate_ready_and_converged(&self) -> anyhow::Result<ChainSnapshot> {
        let snapshot = self.snapshot()?;
        anyhow::ensure!(
            snapshot.height >= BOOTSTRAP_HEIGHT,
            "bootstrap is incomplete (node1 height {}, need at least {BOOTSTRAP_HEIGHT})",
            snapshot.height
        );
        anyhow::ensure!(
            all_tips_match(&snapshot),
            "nodes must be converged before partitioning"
        );
        Ok(snapshot)
    }

    fn disconnect_target_peers(&self, node: MinerNode) -> anyhow::Result<()> {
        let client = create_client(self.url(node))?;
        let peers: Vec<Value> = client.call("getpeerinfo", &[])?;
        for address in peers
            .iter()
            .filter_map(|peer| peer.get("addr").and_then(Value::as_str))
        {
            // Peers can disappear between the status call and disconnect.
            // The bounded isolation witness below is authoritative.
            if let Err(error) = client.call::<Value>("disconnectnode", &[json!(address)]) {
                tracing::debug!(%node, %address, "peer disconnect raced or failed: {error}");
            }
        }
        Ok(())
    }

    fn wait_for_isolation(
        &self,
        node: MinerNode,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<Value> {
        let started = Instant::now();
        let main = Self::main_node(node);
        loop {
            if control.abort_requested() {
                anyhow::bail!("partition aborted while waiting for isolation");
            }
            let target_peers = Self::peer_count(self.url(node));
            let node1_peers = Self::p2p_peer_count(&self.node1_url);
            let main_peers = Self::p2p_peer_count(self.url(main));
            if let (Ok(0), Ok(node1_peers), Ok(main_peers)) =
                (target_peers, node1_peers, main_peers)
            {
                if node1_peers > 0 && main_peers > 0 {
                    return Ok(json!({
                        "isolated_node": node.short_name(),
                        "main_node": main.short_name(),
                        "target_peers": 0,
                        "node1_peers": node1_peers,
                        "main_peers": main_peers
                    }));
                }
            }
            if started.elapsed() >= self.peer_timeout {
                anyhow::bail!(
                    "P2P split did not settle within {} seconds",
                    self.peer_timeout.as_secs()
                );
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn reconnect_target(&self, node: MinerNode) -> anyhow::Result<()> {
        let target_alias = format!("{}-p2p:18444", node.short_name());
        let main = Self::main_node(node);
        let main_alias = format!("{}-p2p:18444", main.short_name());
        let attempts = [
            (&self.node1_url, target_alias.as_str()),
            (self.url(main), target_alias.as_str()),
            (self.url(node), "node1-p2p:18444"),
            (self.url(node), main_alias.as_str()),
        ];
        for (url, peer) in attempts {
            let client = create_client(url)?;
            if let Err(error) = client.call::<Value>("addnode", &[json!(peer), json!("onetry")]) {
                tracing::debug!(%peer, "reconnect attempt failed: {error}");
            }
        }
        Ok(())
    }

    fn wait_for_convergence(
        &self,
        expected_hash: Option<&str>,
        control: &dyn ScenarioControl,
    ) -> anyhow::Result<ChainSnapshot> {
        let started = Instant::now();
        loop {
            let snapshot = self.snapshot();
            if let Ok(snapshot) = snapshot {
                if all_tips_match(&snapshot) {
                    if let Some(expected) = expected_hash {
                        anyhow::ensure!(
                            snapshot.best_hash == expected,
                            "nodes converged on unexpected tip {} (expected {expected})",
                            snapshot.best_hash
                        );
                    }
                    return Ok(snapshot);
                }
            }
            if control.abort_requested() {
                // Healing is not cooperatively abortable: once impairment is
                // clear, convergence remains a required safety boundary.
                tracing::debug!("abort remains pending while waiting for safe convergence");
            }
            if started.elapsed() >= self.convergence_timeout {
                anyhow::bail!(
                    "nodes did not converge within {} seconds",
                    self.convergence_timeout.as_secs()
                );
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
}

fn all_tips_match(snapshot: &ChainSnapshot) -> bool {
    snapshot
        .tips
        .values()
        .all(|hash| hash == &snapshot.best_hash)
}

fn is_p2p_address(address: &str) -> bool {
    address.starts_with("172.30.0.")
        || ["node1-p2p:", "node2-p2p:", "node3-p2p:"]
            .iter()
            .any(|prefix| address.starts_with(prefix))
}

fn duration_env(key: &str, default: u64) -> anyhow::Result<Duration> {
    let seconds = std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.parse::<u64>())
        .transpose()
        .with_context(|| format!("{key} must be a positive integer"))?
        .unwrap_or(default);
    anyhow::ensure!(seconds > 0, "{key} must be positive");
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convergence_requires_every_tip_to_match_node1() {
        let converged = ChainSnapshot {
            height: 204,
            best_hash: "a".to_string(),
            tips: BTreeMap::from([
                ("node1".to_string(), "a".to_string()),
                ("node2".to_string(), "a".to_string()),
                ("node3".to_string(), "a".to_string()),
            ]),
        };
        assert!(all_tips_match(&converged));
        let mut split = converged;
        split.tips.insert("node3".to_string(), "b".to_string());
        assert!(!all_tips_match(&split));
    }

    #[test]
    fn target_and_main_are_complements() {
        assert_eq!(
            RpcNetworkActionBackend::main_node(MinerNode::Node2),
            MinerNode::Node3
        );
        assert_eq!(
            RpcNetworkActionBackend::main_node(MinerNode::Node3),
            MinerNode::Node2
        );
    }

    #[test]
    fn p2p_address_filter_excludes_control_and_host_peers() {
        assert!(is_p2p_address("172.30.0.3:18444"));
        assert!(is_p2p_address("node2-p2p:18444"));
        assert!(!is_p2p_address("172.29.0.3:18444"));
        assert!(!is_p2p_address("127.0.0.1:28444"));
    }
}
