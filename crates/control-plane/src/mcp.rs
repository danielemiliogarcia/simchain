//! MCP interface: the same operations as the HTTP API, exposed over
//! streamable HTTP for coding agents. Thin adapter over `service`/`apply` —
//! tool results carry exactly the JSON payloads the HTTP endpoints return,
//! including the error envelope.

use crate::apply::{apply, ApplyRequest};
use crate::service::{
    abort_job as abort_job_service, config, get_job as get_job_service,
    list_jobs as list_jobs_service, schema, set_mining_state as set_mining_state_service,
    set_spam_state as set_spam_state_service, start_reorg as start_reorg_service,
    start_scenario as start_scenario_service, status, ServiceError,
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

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetSpamStateParams {
    /// Desired manual state: `running` or `paused`.
    pub state: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct StartReorgParams {
    /// Number of tip blocks to replace (1-100).
    #[serde(default = "default_reorg_depth")]
    pub depth: u64,
    /// Mine empty replacement blocks, leaving orphaned transactions pending.
    #[serde(default)]
    pub empty: bool,
    /// Replacement-chain miner: `node2` or `node3`.
    #[serde(default = "default_reorg_node")]
    pub node: String,
    /// Fresh wallet transactions to add to a non-empty replacement.
    #[serde(default)]
    pub adds_new_txs: u64,
    /// Percentage of eligible orphaned wallet transactions to conflict.
    #[serde(default)]
    pub double_spend_pct: u8,
    /// Optional retry key. Reusing it with the same request returns the original job.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

fn default_reorg_depth() -> u64 {
    3
}

fn default_reorg_node() -> String {
    "node3".to_string()
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct JobIdParams {
    /// Server-assigned job identifier.
    pub job_id: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct StartScenarioParams {
    /// Complete validated version-1 scenario document as YAML text.
    pub yaml: String,
    /// Optional retry key. Reusing it with identical YAML returns the original job.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReleaseCheckpointParams {
    /// Server-assigned scenario job identifier.
    pub job_id: String,
    /// URL-safe checkpoint name from the submitted scenario.
    pub checkpoint: String,
    /// Generation returned when this checkpoint occurrence was reached.
    pub generation: u64,
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
        description = "Patch desired runtime configuration with optional generation compare-and-swap. Both workers apply at safe points; structural spam changes reconcile a replacement engine before commit.",
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

    #[tool(
        name = "set_spam_state",
        description = "Set manual desired spam state to running or paused. Pause waits for a consistent cooperative safe point and is idempotent.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn set_spam_state(
        &self,
        Parameters(params): Parameters<SetSpamStateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let desired = match params.state.as_str() {
            "running" => simchain_common::internal_api::DesiredState::Running,
            "paused" => simchain_common::internal_api::DesiredState::Paused,
            _ => {
                return error_json(ServiceError::new(
                    crate::service::ErrorCode::ValidationFailed,
                    "spam state must be running or paused",
                ));
            }
        };
        let app = self.app.clone();
        match tokio::task::spawn_blocking(move || set_spam_state_service(&app, desired))
            .await
            .map_err(join_error)?
        {
            Ok(response) => success_json(&response),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        name = "start_reorg",
        description = "Start one bounded server-side reorg job. The coordinator pauses mining and spam with owned leases, rewrites history through Bitcoin RPC, and requires node1 witness convergence before cleanup.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn start_reorg(
        &self,
        Parameters(params): Parameters<StartReorgParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let app = self.app.clone();
        let request = simchain_common::control_api::ReorgJobRequest {
            depth: params.depth,
            empty: params.empty,
            node: params.node,
            adds_new_txs: params.adds_new_txs,
            double_spend_pct: params.double_spend_pct,
        };
        match tokio::task::spawn_blocking(move || {
            start_reorg_service(&app, request, params.idempotency_key)
        })
        .await
        .map_err(join_error)?
        {
            Ok(response) => success_json(&response),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        name = "start_scenario",
        description = "Validate and start a durable server-side scenario job from YAML. Execution continues independently of the MCP client and may pause at bounded named checkpoints.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn start_scenario(
        &self,
        Parameters(params): Parameters<StartScenarioParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let app = self.app.clone();
        match tokio::task::spawn_blocking(move || {
            start_scenario_service(&app, params.yaml, params.idempotency_key)
        })
        .await
        .map_err(join_error)?
        {
            Ok(response) => success_json(&response),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        name = "get_job",
        description = "Get one job's normalized request, state, phase, leases, result or failure, and separate cleanup outcome.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn get_job(
        &self,
        Parameters(params): Parameters<JobIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match get_job_service(&self.app, &params.job_id) {
            Ok(job) => success_json(&job),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        name = "list_jobs",
        description = "List the active mutation job and bounded recent job history, newest first.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn list_jobs(&self) -> Result<CallToolResult, ErrorData> {
        success_json(&list_jobs_service(&self.app))
    }

    #[tool(
        name = "abort_job",
        description = "Request cooperative abort of a job. A reorg that already changed history finishes its minimum safe rewrite and owned-resource cleanup before becoming terminal.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn abort_job(
        &self,
        Parameters(params): Parameters<JobIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match abort_job_service(&self.app, &params.job_id) {
            Ok(response) => success_json(&response),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        name = "release_checkpoint",
        description = "Release one reached pausing checkpoint using its current generation. Repeating the same release is idempotent; stale generations are rejected.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn release_checkpoint(
        &self,
        Parameters(params): Parameters<ReleaseCheckpointParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match crate::service::release_checkpoint(
            &self.app,
            &params.job_id,
            &params.checkpoint,
            simchain_common::control_api::ReleaseCheckpointRequest {
                generation: params.generation,
            },
        ) {
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
             desired runtime configuration and bounded mutation jobs. Start with \
             get_status and get_config_schema. Jobs are asynchronous: start one, \
             then inspect it with get_job or list_jobs; scenario checkpoints require \
             their returned generation when released."
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
