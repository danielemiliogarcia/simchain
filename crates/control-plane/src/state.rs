//! Control-plane bootstrap configuration and shared application state.
//!
//! Boot-only infrastructure stays in the process environment. Runtime mining
//! and spam intent is loaded from the narrow control-state directory.

use crate::backend::{
    ChainBackend, MiningControlBackend, NetworkControlBackend, SpamControlBackend,
};
use crate::control_state::{ControlState, ControlStateStore};
use crate::jobs::JobManager;
use crate::status::StatusSnapshot;
use std::fs::File;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

pub const MINING_COMPONENT: &str = "mining";
pub const SPAM_COMPONENT: &str = "spam";
pub const NODE1_COMPONENT: &str = "node1";
pub const NODE2_COMPONENT: &str = "node2";
pub const NODE3_COMPONENT: &str = "node3";

#[derive(Clone, Debug)]
pub struct ControlPlaneConfig {
    pub listen_addr: SocketAddr,
    pub node1_url: String,
    pub node2_url: String,
    pub node3_url: String,
    pub state_dir: PathBuf,
    pub mining_control_url: String,
    pub spam_control_url: String,
    pub node1_network_agent_url: String,
    pub node2_network_agent_url: String,
    pub node3_network_agent_url: String,
    pub internal_token: String,
    pub explorer_url: String,
    pub explorer_probe_url: String,
    pub node2_wallet_name: String,
    pub node3_wallet_name: String,
    pub faucet_wallet_reserve_sats: u64,
    pub faucet_max_request_sats: u64,
}

impl ControlPlaneConfig {
    /// Read control-plane bootstrap settings from the process environment.
    pub fn from_process_env() -> anyhow::Result<Self> {
        let listen_addr = std::env::var("CONTROL_PLANE_LISTEN_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8090".to_string())
            .parse::<SocketAddr>()
            .map_err(|error| anyhow::anyhow!("invalid CONTROL_PLANE_LISTEN_ADDR: {error}"))?;
        let node1_url = std::env::var("NODE1_RPC_URL")
            .unwrap_or_else(|_| "http://btc-simnet-node1:18443".to_string());
        let node2_url = std::env::var("NODE2_RPC_URL")
            .unwrap_or_else(|_| "http://btc-simnet-node2:18443".to_string());
        let node3_url = std::env::var("NODE3_RPC_URL")
            .unwrap_or_else(|_| "http://btc-simnet-node3:18443".to_string());
        let state_dir = std::env::var("SIMCHAIN_CONTROL_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(".simchain-control"));
        let mining_control_url = std::env::var("MINING_CONTROL_URL")
            .unwrap_or_else(|_| "http://btc-simnet-mining-controller:9081".to_string());
        let spam_control_url = std::env::var("SPAM_CONTROL_URL")
            .unwrap_or_else(|_| "http://btc-simnet-spammer:9082".to_string());
        let node1_network_agent_url = std::env::var("NODE1_NETWORK_AGENT_URL")
            .unwrap_or_else(|_| "http://btc-simnet-node1:9083".to_string());
        let node2_network_agent_url = std::env::var("NODE2_NETWORK_AGENT_URL")
            .unwrap_or_else(|_| "http://btc-simnet-node2:9083".to_string());
        let node3_network_agent_url = std::env::var("NODE3_NETWORK_AGENT_URL")
            .unwrap_or_else(|_| "http://btc-simnet-node3:9083".to_string());
        let internal_token = std::env::var("SIMCHAIN_INTERNAL_TOKEN")
            .unwrap_or_else(|_| "simchain-internal-dev-token".to_string());
        if internal_token.trim().is_empty() {
            anyhow::bail!("SIMCHAIN_INTERNAL_TOKEN must not be empty");
        }
        let explorer_url = match std::env::var("MEMPOOL_WEB_URL") {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                let port = std::env::var("MEMPOOL_WEB_PORT")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "1080".to_string())
                    .parse::<u16>()
                    .map_err(|error| anyhow::anyhow!("invalid MEMPOOL_WEB_PORT: {error}"))?;
                format!("http://127.0.0.1:{port}")
            }
        };
        ensure_http_url("MEMPOOL_WEB_URL", &explorer_url)?;
        let explorer_probe_url = std::env::var("MEMPOOL_WEB_INTERNAL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| explorer_url.clone());
        ensure_http_url("MEMPOOL_WEB_INTERNAL_URL", &explorer_probe_url)?;
        let node2_wallet_name = non_empty_env("NODE2_WALLET_NAME", "node2")?;
        let node3_wallet_name = non_empty_env("NODE3_WALLET_NAME", "node3")?;
        let faucet_wallet_reserve_sats = exact_btc_env("FAUCET_WALLET_RESERVE_BTC", "600")?;
        let faucet_max_request_sats = exact_btc_env("FAUCET_MAX_REQUEST_BTC", "100")?;
        anyhow::ensure!(
            faucet_max_request_sats > 0,
            "FAUCET_MAX_REQUEST_BTC must be positive"
        );
        Ok(Self {
            listen_addr,
            node1_url,
            node2_url,
            node3_url,
            state_dir,
            mining_control_url,
            spam_control_url,
            node1_network_agent_url,
            node2_network_agent_url,
            node3_network_agent_url,
            internal_token,
            explorer_url,
            explorer_probe_url,
            node2_wallet_name,
            node3_wallet_name,
            faucet_wallet_reserve_sats,
            faucet_max_request_sats,
        })
    }
}

fn non_empty_env(key: &str, default: &str) -> anyhow::Result<String> {
    let value = std::env::var(key).unwrap_or_else(|_| default.to_string());
    let value = value.trim();
    anyhow::ensure!(!value.is_empty(), "{key} must not be empty");
    Ok(value.to_string())
}

fn exact_btc_env(key: &str, default: &str) -> anyhow::Result<u64> {
    let value = std::env::var(key).unwrap_or_else(|_| default.to_string());
    simchain_common::parse_btc_sats(&value)
        .map_err(|error| anyhow::anyhow!("invalid {key}: {error}"))
}

fn ensure_http_url(key: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.starts_with("http://") || value.starts_with("https://"),
        "{key} must use http:// or https://"
    );
    Ok(())
}

pub struct AppState {
    pub config: ControlPlaneConfig,
    pub token: String,
    pub chain: Arc<dyn ChainBackend>,
    pub mining: Arc<dyn MiningControlBackend>,
    pub spam: Arc<dyn SpamControlBackend>,
    pub network: Arc<dyn NetworkControlBackend>,
    pub jobs: Arc<JobManager>,
    pub control_state: Arc<RwLock<ControlState>>,
    pub control_store: ControlStateStore,
    pub status: RwLock<StatusSnapshot>,
    /// Held for the process lifetime: job coordination is deliberately
    /// single-instance even though desired-state writes also use a short lock.
    pub _instance_guard: File,
    /// Serializes applies within this process; the on-disk flock serializes
    /// across processes.
    pub apply_lock: Arc<Mutex<()>>,
}

pub type SharedState = Arc<AppState>;
