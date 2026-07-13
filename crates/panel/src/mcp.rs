//! MCP interface: the same four operations as the HTTP API, exposed over
//! streamable HTTP for coding agents. Thin adapter over `service`/`apply` —
//! tool results carry exactly the JSON payloads the HTTP endpoints return,
//! including the error envelope.

use crate::apply::{apply, ApplyRequest};
use crate::service::{schema, settings_state, status, ServiceError};
use crate::state::SharedState;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct PanelMcp {
    app: SharedState,
}

fn success_json(value: &impl serde::Serialize) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

/// Service-layer failures are tool-level errors: the agent should see the
/// same `{"error": {code, message, details}}` envelope the HTTP API returns.
fn error_json(error: ServiceError) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        error.envelope().to_string(),
    )]))
}

fn join_error(error: tokio::task::JoinError) -> ErrorData {
    ErrorData::internal_error(format!("panel worker task failed: {error}"), None)
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ApplySettingsParams {
    /// Partial map of panel-managed env vars to new values. Only the keys to
    /// change are needed; empty unsets optional settings and resets required
    /// settings to their default.
    pub settings: BTreeMap<String, String>,
    /// Optional revision from get_settings; when provided and stale the
    /// apply is rejected with the stale_revision error code. Omit it to
    /// merge against the current staged values.
    #[serde(default)]
    pub base_revision: Option<String>,
}

#[tool_router(vis = "pub(crate)")]
impl PanelMcp {
    pub fn new(app: SharedState) -> Self {
        Self { app }
    }

    #[tool(
        name = "get_status",
        description = "Live simnet status from node1: chain height, best block hash, recent blocks with cadence, mempool depth and fee histogram, plus tool-container states.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn get_status(&self) -> Result<CallToolResult, ErrorData> {
        success_json(&status(&self.app))
    }

    #[tool(
        name = "get_settings",
        description = "The panel-managed live-retune settings: staged values from .env plus defaults, the values each running tool container was started with, the revision needed for compare-and-swap applies, and which services a current apply would recreate.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn get_settings(&self) -> Result<CallToolResult, ErrorData> {
        let app = self.app.clone();
        match tokio::task::spawn_blocking(move || settings_state(&app))
            .await
            .map_err(join_error)?
        {
            Ok(view) => success_json(&view),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        name = "get_setting_schema",
        description = "The catalog of panel-managed settings: name, default, group, control type, which compose service a change recreates, help text and caveats. Use it to discover valid knobs before apply_settings.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn get_setting_schema(&self) -> Result<CallToolResult, ErrorData> {
        success_json(&schema())
    }

    #[tool(
        name = "apply_settings",
        description = "Apply a live retune: rewrites the repo .env (managed keys are canonicalized into one panel-managed block) and recreates only the affected tool containers (mining controller and/or spammer) with rollback on failure. The chain and node containers are never touched. Caveat: FALLBACK_FEE is shared with the nodes' -fallbackfee; a live apply moves the spam fee floor immediately, but node wallets keep their old fallback until the nodes are recreated outside the panel.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn apply_settings(
        &self,
        Parameters(params): Parameters<ApplySettingsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let app = self.app.clone();
        let request = ApplyRequest {
            settings: params.settings,
            base_revision: params.base_revision,
        };
        match tokio::task::spawn_blocking(move || apply(&app, request))
            .await
            .map_err(join_error)?
        {
            Ok(report) => success_json(&report),
            Err(error) => error_json(error),
        }
    }
}

#[tool_handler]
impl ServerHandler for PanelMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "Simchain control panel: inspect the live regtest simnet and retune the \
             mining controller and spammer without restarting the chain. Start with \
             get_setting_schema to discover the knobs, get_settings for current staged \
             vs running values, then apply_settings with just the keys to change."
                .to_string(),
        );
        info
    }
}

/// The `/mcp` tower service, mounted into the panel's axum router. Sessions
/// are in-memory; the panel is single-instance and localhost-only.
pub fn mcp_service(app: SharedState) -> StreamableHttpService<PanelMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(PanelMcp::new(app.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}
