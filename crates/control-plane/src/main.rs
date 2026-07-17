//! Simchain dashboard and control-plane entry point.

mod api;
mod apply;
mod backend;
mod control_state;
mod faucet_job;
mod faucet_store;
mod internal_client;
mod job_store;
mod jobs;
mod mcp;
mod network_job;
mod reconcile;
mod reorg_job;
mod rpc_backend;
mod scenario_job;
mod service;
mod state;
mod status;
mod storage;
#[cfg(test)]
mod test_support;

use rand::RngCore;
use simchain_common::config::CommonConfig;
use state::{AppState, ControlPlaneConfig};
use std::sync::{Arc, Mutex, RwLock};

fn load_or_create_token(config: &ControlPlaneConfig) -> anyhow::Result<String> {
    let configured = std::env::var("CONTROL_PLANE_API_TOKEN").ok();
    load_or_create_token_from(config, configured.as_deref())
}

fn load_or_create_token_from(
    config: &ControlPlaneConfig,
    configured: Option<&str>,
) -> anyhow::Result<String> {
    let path = config.state_dir.join("token");
    let configured = configured
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty());
    let existing = std::fs::read_to_string(&path)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty());

    let (token, generated) = match (configured, existing) {
        (Some(token), _) => (token, false),
        (None, Some(token)) => (token, false),
        (None, None) => {
            let mut bytes = [0u8; 32];
            rand::rng().fill_bytes(&mut bytes);
            (
                bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
                true,
            )
        }
    };

    // Always synchronize the effective token to the documented host-visible
    // file. This also repairs a stale file after the configured token changes
    // and reasserts host ownership plus mode 0600 on every startup.
    let ownership = storage::dir_ownership(&config.state_dir, 0o600)?;
    storage::write_atomic(&path, &token, ownership)?;
    if generated {
        tracing::info!(
            "generated a new control-plane API token at {}",
            path.display()
        );
    } else {
        tracing::info!("synchronized control-plane API token at {}", path.display());
    }
    Ok(token)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    simchain_common::init_tracing("simchain_control_plane=info,info");
    CommonConfig::init()?;
    let config = ControlPlaneConfig::from_process_env()?;
    let store = control_state::ControlStateStore::open(config.state_dir.clone())?;
    let instance_guard = store.try_instance_lock()?.ok_or_else(|| {
        anyhow::anyhow!("another control-plane process already owns this state directory")
    })?;
    let token_guard = store.try_apply_lock()?.ok_or_else(|| {
        anyhow::anyhow!("another control-plane process is mutating durable state during startup")
    })?;
    let token = load_or_create_token(&config)?;
    drop(token_guard);

    let mining = Arc::new(internal_client::MiningClient::new(
        config.mining_control_url.clone(),
        config.internal_token.clone(),
    ));
    let spam = Arc::new(internal_client::SpamClient::new(
        config.spam_control_url.clone(),
        config.internal_token.clone(),
    ));
    let network = Arc::new(internal_client::NetworkClients::new(
        config.node1_network_agent_url.clone(),
        config.node2_network_agent_url.clone(),
        config.node3_network_agent_url.clone(),
        config.internal_token.clone(),
    ));
    let chain = Arc::new(rpc_backend::RpcChainBackend::new(
        config.node1_url.clone(),
        config.node2_url.clone(),
        config.node3_url.clone(),
    ));
    let control_state = Arc::new(RwLock::new(
        store.load_or_initialize(control_state::desired_from_process_env()?)?,
    ));
    let apply_lock = Arc::new(Mutex::new(()));
    let reorg_executor = Arc::new(reorg_job::RpcReorgExecutor::from_config(&config)?);
    let scenario_backend = Arc::new(scenario_job::RpcScenarioActionBackend::from_config(
        &config,
        mining.clone(),
        spam.clone(),
    )?);
    let network_actions = Arc::new(network_job::RpcNetworkActionBackend::from_config(&config)?);
    let faucet = Arc::new(faucet_job::RpcFaucetBackend::from_config(&config)?);
    let jobs = jobs::JobManager::open(
        &config.state_dir,
        jobs::JobDependencies {
            mining: mining.clone(),
            spam: spam.clone(),
            network: network.clone(),
            chain: chain.clone(),
            control_store: store.clone(),
            control_state: control_state.clone(),
            apply_lock: apply_lock.clone(),
            reorg: reorg_executor,
            scenario: scenario_backend,
            network_actions,
            faucet,
            faucet_settings: jobs::FaucetSettings {
                node2_wallet_name: config.node2_wallet_name.clone(),
                node3_wallet_name: config.node3_wallet_name.clone(),
                wallet_reserve_sats: config.faucet_wallet_reserve_sats,
                max_request_sats: config.faucet_max_request_sats,
                explorer_url: config.explorer_url.clone(),
            },
        },
    )?;
    let app = Arc::new(AppState {
        config: config.clone(),
        token,
        chain,
        mining,
        spam,
        network,
        jobs,
        control_state,
        control_store: store,
        status: RwLock::new(status::StatusSnapshot::default()),
        _instance_guard: instance_guard,
        apply_lock,
    });

    status::spawn_sampler(app.clone());
    reconcile::spawn(app.clone());

    let router = api::router(app);
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!(
        "control plane listening on {} (state directory {})",
        config.listen_addr,
        config.state_dir.display()
    );
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod token_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn config(dir: &std::path::Path) -> ControlPlaneConfig {
        std::fs::create_dir_all(dir.join(".simchain-control")).expect("state dir");
        ControlPlaneConfig {
            listen_addr: "127.0.0.1:0".parse().expect("address"),
            node1_url: "http://node1:18443".to_string(),
            node2_url: "http://node2:18443".to_string(),
            node3_url: "http://node3:18443".to_string(),
            state_dir: dir.join(".simchain-control"),
            mining_control_url: "http://mining:9081".to_string(),
            spam_control_url: "http://spam:9082".to_string(),
            node1_network_agent_url: "http://node1:9083".to_string(),
            node2_network_agent_url: "http://node2:9083".to_string(),
            node3_network_agent_url: "http://node3:9083".to_string(),
            internal_token: "test-internal-token".to_string(),
            explorer_url: "http://127.0.0.1:1080".to_string(),
            explorer_probe_url: "http://mempool-web:8080".to_string(),
            node2_wallet_name: "node2".to_string(),
            node3_wallet_name: "node3".to_string(),
            faucet_wallet_reserve_sats: 60_000_000_000,
            faucet_max_request_sats: 10_000_000_000,
        }
    }

    #[test]
    fn configured_token_replaces_stale_file_and_keeps_it_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".simchain-control/token");
        std::fs::create_dir_all(dir.path().join(".simchain-control")).expect("state dir");
        std::fs::write(&path, "stale").expect("seed token");
        let token =
            load_or_create_token_from(&config(dir.path()), Some("configured")).expect("load token");
        assert_eq!(token, "configured");
        assert_eq!(
            std::fs::read_to_string(&path).expect("token file"),
            "configured"
        );
        assert_eq!(
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn existing_generated_token_is_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".simchain-control")).expect("state dir");
        std::fs::write(dir.path().join(".simchain-control/token"), "existing\n")
            .expect("seed token");
        let token = load_or_create_token_from(&config(dir.path()), None).expect("load token");
        assert_eq!(token, "existing");
    }
}
