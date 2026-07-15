//! Simchain dashboard / control panel entry point.
//!
//! Note: unlike the other tools this binary deliberately does NOT call
//! `dotenvy::dotenv()`. Loading `.env` into the process environment would
//! leak managed values into the nested `docker compose` invocations, where
//! shell env overrides the project `.env` — exactly the file this panel
//! rewrites. `.env` is only ever parsed into in-memory maps.

mod api;
mod apply;
mod backend;
mod compose;
mod control_state;
mod docker_inspect;
mod envfile;
mod hybrid_backend;
mod internal_client;
mod job_store;
mod jobs;
mod mcp;
mod reconcile;
mod reorg_job;
mod service;
mod state;
mod status;
#[cfg(test)]
mod test_support;

use rand::RngCore;
use simchain_common::config::CommonConfig;
use state::{AppState, ControlPlaneConfig};
use std::sync::{Arc, Mutex, RwLock};

fn load_or_create_token(config: &ControlPlaneConfig) -> anyhow::Result<String> {
    let configured = std::env::var("CONTROL_PLANE_API_TOKEN")
        .or_else(|_| std::env::var("PANEL_API_TOKEN"))
        .ok();
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

    let legacy = std::fs::read_to_string(config.repo_root.join(".panel-token"))
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty());

    let (token, generated) = match (configured, existing, legacy) {
        (Some(token), _, _) => (token, false),
        (None, Some(token), _) | (None, None, Some(token)) => (token, false),
        (None, None, None) => {
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
    let ownership = envfile::dir_ownership(&config.state_dir, 0o600)?;
    envfile::write_atomic(&path, &token, ownership)?;
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
    let token = load_or_create_token(&config)?;

    let legacy = compose::ComposeBackend::new(
        config.repo_root.clone(),
        config.env_file.clone(),
        config.compose_project.clone(),
        vec![
            config.node1_url.clone(),
            config.node2_url.clone(),
            config.node3_url.clone(),
        ],
    );
    let mining = Arc::new(internal_client::MiningClient::new(
        config.mining_control_url.clone(),
        config.internal_token.clone(),
    ));
    let spam = Arc::new(internal_client::SpamClient::new(
        config.spam_control_url.clone(),
        config.internal_token.clone(),
    ));
    let backend = Arc::new(hybrid_backend::HybridBackend::new(
        legacy,
        mining.clone(),
        spam.clone(),
    ));
    let control_state =
        store.load_or_initialize(control_state::desired_from_legacy_env(&config.env_file)?)?;
    let reorg_executor = Arc::new(reorg_job::RpcReorgExecutor::from_config(&config)?);
    let jobs = jobs::JobManager::open(
        &config.state_dir,
        mining.clone(),
        spam.clone(),
        reorg_executor,
    )?;
    let app = Arc::new(AppState {
        config: config.clone(),
        token,
        components: backend.clone(),
        configuration: backend.clone(),
        job_actions: backend,
        mining,
        spam,
        jobs,
        control_state: RwLock::new(control_state),
        control_store: store,
        status: RwLock::new(status::StatusSnapshot::default()),
        apply_lock: Mutex::new(()),
    });

    status::spawn_sampler(app.clone());
    reconcile::spawn(app.clone());

    let router = api::router(app);
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!(
        "control plane listening on {} (compose project {}, env file {})",
        config.listen_addr,
        config.compose_project,
        config.env_file.display()
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
            repo_root: dir.to_path_buf(),
            env_file: dir.join(".env"),
            compose_project: "simchain".to_string(),
            node1_url: "http://node1:18443".to_string(),
            node2_url: "http://node2:18443".to_string(),
            node3_url: "http://node3:18443".to_string(),
            state_dir: dir.join(".simchain-control"),
            mining_control_url: "http://mining:9081".to_string(),
            spam_control_url: "http://spam:9082".to_string(),
            internal_token: "test-internal-token".to_string(),
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
