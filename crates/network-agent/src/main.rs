//! Namespace-local, lease-protected P2P impairment agent.

mod control;
mod server;
mod system;

use control::NetworkControl;
use std::net::SocketAddr;
use std::sync::Arc;
use system::CommandNetworkSystem;

fn main() -> anyhow::Result<()> {
    simchain_common::init_tracing("simchain_network_agent=info,info");
    let listen_addr = env_or("NETWORK_AGENT_LISTEN_ADDR", "0.0.0.0:9083")
        .parse::<SocketAddr>()
        .map_err(|error| anyhow::anyhow!("invalid NETWORK_AGENT_LISTEN_ADDR: {error}"))?;
    let token = env_or("SIMCHAIN_INTERNAL_TOKEN", "simchain-internal-dev-token");
    anyhow::ensure!(
        !token.trim().is_empty(),
        "SIMCHAIN_INTERNAL_TOKEN must not be empty"
    );
    let node = std::env::var("NETWORK_AGENT_NODE")
        .map_err(|_| anyhow::anyhow!("NETWORK_AGENT_NODE is required"))?;
    anyhow::ensure!(
        !node.trim().is_empty(),
        "NETWORK_AGENT_NODE must not be empty"
    );
    let probe_ip = env_or("P2P_PROBE_IP", "172.30.0.254");
    let system = Arc::new(CommandNetworkSystem::new(probe_ip));
    let control = Arc::new(NetworkControl::new(node, system)?);
    control.spawn_expiry_worker()?;
    server::run(listen_addr, token, control)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}
