//! Panel configuration (from process env, never from `.env`) and the shared
//! application state.
//!
//! The panel deliberately does NOT load `.env` into its own process
//! environment: compose gives shell variables precedence over the project
//! `.env`, so leaked managed values would override the very file the panel
//! rewrites (see the plan's finding 1). `.env` is only ever parsed into
//! in-memory maps.

use crate::compose::Executor;
use crate::status::StatusSnapshot;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

pub const CONTROLLER_CONTAINER: &str = "btc-simnet-mining-controller";
pub const SPAMMER_CONTAINER: &str = "btc-simnet-spammer";
pub const NODE1_CONTAINER: &str = "btc-simnet-node1";
pub const NODE2_CONTAINER: &str = "btc-simnet-node2";
pub const NODE3_CONTAINER: &str = "btc-simnet-node3";

#[derive(Clone, Debug)]
pub struct PanelConfig {
    pub listen_addr: SocketAddr,
    pub repo_root: PathBuf,
    pub env_file: PathBuf,
    pub compose_project: String,
    pub node1_url: String,
    pub node2_url: String,
    pub node3_url: String,
}

impl PanelConfig {
    /// Read the panel bootstrap settings from the process environment only.
    pub fn from_process_env() -> anyhow::Result<Self> {
        let listen_addr = std::env::var("PANEL_LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse::<SocketAddr>()
            .map_err(|error| anyhow::anyhow!("invalid PANEL_LISTEN_ADDR: {error}"))?;
        let repo_root =
            PathBuf::from(std::env::var("SIMCHAIN_REPO_ROOT").unwrap_or_else(|_| ".".to_string()));
        let env_file = std::env::var("SIMCHAIN_ENV_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root.join(".env"));
        let compose_project = match std::env::var("COMPOSE_PROJECT_NAME") {
            Ok(value) if !value.trim().is_empty() => value,
            _ => "simchain".to_string(),
        };
        let node1_url = std::env::var("NODE1_RPC_URL")
            .unwrap_or_else(|_| "http://btc-simnet-node1:18443".to_string());
        let node2_url = std::env::var("NODE2_RPC_URL")
            .unwrap_or_else(|_| "http://btc-simnet-node2:18443".to_string());
        let node3_url = std::env::var("NODE3_RPC_URL")
            .unwrap_or_else(|_| "http://btc-simnet-node3:18443".to_string());
        Ok(Self {
            listen_addr,
            repo_root,
            env_file,
            compose_project,
            node1_url,
            node2_url,
            node3_url,
        })
    }
}

pub struct AppState {
    pub config: PanelConfig,
    pub token: String,
    pub executor: Arc<dyn Executor>,
    pub status: RwLock<StatusSnapshot>,
    /// Serializes applies within this process; the on-disk flock serializes
    /// across processes.
    pub apply_lock: Mutex<()>,
}

pub type SharedState = Arc<AppState>;
