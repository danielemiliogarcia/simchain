//! MCP interface: the same operations as the HTTP API, exposed over
//! streamable HTTP for coding agents. Thin adapter over `service`/`apply` —
//! tool results carry exactly the JSON payloads the HTTP endpoints return,
//! including the error envelope.

use crate::apply::{apply, ApplyRequest};
use crate::service::{
    config, schema, set_mining_state as set_mining_state_service, status, ServiceError,
};
use crate::state::SharedState;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct ControlPlaneMcp {
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
    let envelope = serde_json::to_string(&error.envelope())
        .map_err(|serialize_error| ErrorData::internal_error(serialize_error.to_string(), None))?;
    Ok(CallToolResult::error(vec![ContentBlock::text(envelope)]))
}

fn join_error(error: tokio::task::JoinError) -> ErrorData {
    ErrorData::internal_error(format!("control-plane worker task failed: {error}"), None)
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetConfigParams {
    /// Partial map of managed settings to new values. Only the keys to
    /// change are needed; empty unsets optional settings and resets required
    /// settings to their default.
    pub settings: BTreeMap<String, String>,
    /// Optional generation from get_config. A stale generation is rejected.
    #[serde(default)]
    pub base_generation: Option<u64>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetMiningStateParams {
    /// Desired manual state: `running` or `paused`.
    pub state: String,
}

#[tool_router(vis = "pub(crate)")]
impl ControlPlaneMcp {
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
        name = "get_config",
        description = "Get desired and effective runtime configuration, its generation, validation warnings, and pending component applies.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn get_config(&self) -> Result<CallToolResult, ErrorData> {
        let app = self.app.clone();
        match tokio::task::spawn_blocking(move || config(&app))
            .await
            .map_err(join_error)?
        {
            Ok(view) => success_json(&view),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        name = "get_config_schema",
        description = "Get the typed runtime-setting catalog, validation metadata, owning component, and apply classification.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn get_config_schema(&self) -> Result<CallToolResult, ErrorData> {
        success_json(&schema())
    }

    #[tool(
        name = "set_config",
        description = "Patch desired runtime configuration with optional generation compare-and-swap. Mining applies at a worker safe point; spam still uses the transitional component adapter.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn set_config(
        &self,
        Parameters(params): Parameters<SetConfigParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let app = self.app.clone();
        let request = ApplyRequest {
            settings: params.settings,
            base_revision: None,
            base_generation: params.base_generation,
        };
        match tokio::task::spawn_blocking(move || apply(&app, request))
            .await
            .map_err(join_error)?
        {
            Ok(report) => success_json(&report),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        name = "set_mining_state",
        description = "Set manual desired mining state to running or paused. Pause waits for the worker's next safe point and is idempotent.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn set_mining_state(
        &self,
        Parameters(params): Parameters<SetMiningStateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let desired = match params.state.as_str() {
            "running" => simchain_common::internal_api::DesiredState::Running,
            "paused" => simchain_common::internal_api::DesiredState::Paused,
            _ => {
                return error_json(ServiceError::new(
                    crate::service::ErrorCode::ValidationFailed,
                    "mining state must be running or paused",
                ));
            }
        };
        let app = self.app.clone();
        match tokio::task::spawn_blocking(move || set_mining_state_service(&app, desired))
            .await
            .map_err(join_error)?
        {
            Ok(response) => success_json(&response),
            Err(error) => error_json(error),
        }
    }
}

#[tool_handler]
impl ServerHandler for ControlPlaneMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "Simchain control plane: inspect the live regtest simnet and manage its \
             desired runtime configuration. Start with get_config_schema, then use \
             get_config and set_config with only the keys to change."
                .to_string(),
        );
        info
    }
}

/// The `/mcp` tower service, mounted into the panel's axum router. Sessions
/// are in-memory; the panel is single-instance and localhost-only.
pub fn mcp_service(
    app: SharedState,
) -> StreamableHttpService<ControlPlaneMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(ControlPlaneMcp::new(app.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}
