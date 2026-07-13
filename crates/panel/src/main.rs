//! Simchain dashboard / control panel entry point.
//!
//! Note: unlike the other tools this binary deliberately does NOT call
//! `dotenvy::dotenv()`. Loading `.env` into the process environment would
//! leak managed values into the nested `docker compose` invocations, where
//! shell env overrides the project `.env` — exactly the file this panel
//! rewrites. `.env` is only ever parsed into in-memory maps.

mod api;
mod apply;
mod compose;
mod docker_inspect;
mod envfile;
mod mcp;
mod service;
mod state;
mod status;
#[cfg(test)]
mod test_support;

use rand::RngCore;
use simchain_common::config::CommonConfig;
use state::{AppState, PanelConfig};
use std::sync::{Arc, Mutex, RwLock};

fn load_or_create_token(config: &PanelConfig) -> anyhow::Result<String> {
    let configured = std::env::var("PANEL_API_TOKEN").ok();
    load_or_create_token_from(config, configured.as_deref())
}

fn load_or_create_token_from(
    config: &PanelConfig,
    configured: Option<&str>,
) -> anyhow::Result<String> {
    let path = config.repo_root.join(".panel-token");
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
    // file. This also repairs a stale file after PANEL_API_TOKEN is changed
    // and reasserts host ownership plus mode 0600 on every startup.
    let ownership = envfile::dir_ownership(&config.repo_root, 0o600)?;
    envfile::write_atomic(&path, &token, ownership)?;
    if generated {
        tracing::info!("generated a new panel API token at {}", path.display());
    } else {
        tracing::info!("synchronized panel API token file at {}", path.display());
    }
    Ok(token)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    simchain_common::init_tracing("simchain_panel=info,info");
    CommonConfig::init()?;
    let config = PanelConfig::from_process_env()?;
    let token = load_or_create_token(&config)?;

    let executor = Arc::new(compose::SystemExecutor::new(
        config.repo_root.clone(),
        config.env_file.clone(),
        config.compose_project.clone(),
        vec![
            config.node1_url.clone(),
            config.node2_url.clone(),
            config.node3_url.clone(),
        ],
    ));
    let app = Arc::new(AppState {
        config: config.clone(),
        token,
        executor,
        status: RwLock::new(status::StatusSnapshot::default()),
        apply_lock: Mutex::new(()),
    });

    status::spawn_sampler(app.clone());

    let router = api::router(app);
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!(
        "panel listening on {} (compose project {}, env file {})",
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

    fn config(dir: &std::path::Path) -> PanelConfig {
        PanelConfig {
            listen_addr: "127.0.0.1:0".parse().expect("address"),
            repo_root: dir.to_path_buf(),
            env_file: dir.join(".env"),
            compose_project: "simchain".to_string(),
            node1_url: "http://node1:18443".to_string(),
            node2_url: "http://node2:18443".to_string(),
            node3_url: "http://node3:18443".to_string(),
        }
    }

    #[test]
    fn configured_token_replaces_stale_file_and_keeps_it_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".panel-token");
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
        std::fs::write(dir.path().join(".panel-token"), "existing\n").expect("seed token");
        let token = load_or_create_token_from(&config(dir.path()), None).expect("load token");
        assert_eq!(token, "existing");
    }
}
