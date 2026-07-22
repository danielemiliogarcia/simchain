//! HTTP surface: versioned JSON API (v1), token auth on mutations, the
//! embedded browser UI, and the `/mcp` mount. Thin adapter over `service`.

use crate::apply::{apply, ApplyRequest};
use crate::service::{
    abort_job, config, faucet_status, faucet_transfer, get_checkpoint, get_job, job_events,
    list_jobs, release_checkpoint, schema, set_mining_state, set_spam_state, start_degrade,
    start_faucet, start_mine, start_partition, start_reorg, start_scenario, start_spam_burst,
    status, ErrorCode, ServiceError,
};
use crate::state::SharedState;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use axum::http::uri::Authority;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use simchain_common::control_api::{
    ConfigResponse, FaucetStatusResponse, FaucetTransfer, JobDetail, JobEventsResponse,
    JobListResponse, StatusResponse,
};
use std::net::IpAddr;
use std::str::FromStr;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLES_CSS: &str = include_str!("../static/styles.css");

pub fn router(app: SharedState) -> Router {
    // The whole MCP endpoint requires the bearer token: an MCP session can
    // reach the mutating apply tool.
    let mcp = Router::new()
        .fallback_service(crate::mcp::mcp_service(app.clone()))
        .layer(middleware::from_fn_with_state(app.clone(), require_token));

    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/styles.css", get(styles_css))
        .route("/health/live", get(live_handler))
        .route("/health/ready", get(ready_handler))
        .route(
            "/api/v1/config",
            get(config_handler).patch(config_patch_handler),
        )
        .route("/api/v1/config/schema", get(schema_handler))
        .route("/api/v1/dashboard", get(dashboard_handler))
        .route("/api/v1/mining/state", put(mining_state_handler))
        .route("/api/v1/spam/state", put(spam_state_handler))
        .route("/api/v1/events", get(global_events_handler))
        .route("/api/v1/jobs", get(jobs_handler))
        .route("/api/v1/jobs/faucet", post(faucet_job_handler))
        .route("/api/v1/jobs/reorg", post(reorg_job_handler))
        .route("/api/v1/jobs/scenario", post(scenario_job_handler))
        .route("/api/v1/jobs/mine", post(mine_job_handler))
        .route("/api/v1/jobs/spam-burst", post(spam_burst_job_handler))
        .route("/api/v1/jobs/partition", post(partition_job_handler))
        .route("/api/v1/jobs/degrade", post(degrade_job_handler))
        .route("/api/v1/faucet", get(faucet_status_handler))
        .route(
            "/api/v1/faucet/transfers/{txid}",
            get(faucet_transfer_handler),
        )
        .route("/api/v1/jobs/{job_id}", get(job_handler))
        .route("/api/v1/jobs/{job_id}/events", get(job_events_handler))
        .route("/api/v1/jobs/{job_id}/abort", post(abort_job_handler))
        .route(
            "/api/v1/jobs/{job_id}/checkpoints/{name}",
            get(checkpoint_handler),
        )
        .route(
            "/api/v1/jobs/{job_id}/checkpoints/{name}/release",
            post(release_checkpoint_handler),
        )
        .route("/api/v1/status", get(status_handler))
        .nest("/mcp", mcp)
        // Binding to loopback is not sufficient against DNS rebinding: a
        // hostile page can resolve its own hostname to 127.0.0.1. Reject any
        // non-loopback Host before serving the page that contains the token.
        .layer(middleware::from_fn(require_loopback_host))
        .with_state(app)
}

async fn require_loopback_host(request: Request, next: Next) -> Response {
    let allowed = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Authority::from_str(value).ok())
        .map(|authority| {
            let host = authority.host();
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
        .unwrap_or(false);
    if allowed {
        next.run(request).await
    } else {
        (
            StatusCode::MISDIRECTED_REQUEST,
            Json(serde_json::json!({
                "error": {
                    "code": "invalid_host",
                    "message": "the control plane accepts only loopback Host headers"
                }
            })),
        )
            .into_response()
    }
}

fn error_response(error: &ServiceError) -> Response {
    let status =
        StatusCode::from_u16(error.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(error.envelope())).into_response()
}

/// Constant-time-ish comparison so the token guard is not a timing oracle.
fn token_matches(expected: &str, provided: &str) -> bool {
    let expected = expected.as_bytes();
    let provided = provided.as_bytes();
    if expected.len() != provided.len() {
        return false;
    }
    expected
        .iter()
        .zip(provided)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn request_has_token(app: &SharedState, request: &Request) -> bool {
    request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|token| token_matches(&app.token, token))
        .unwrap_or(false)
}

async fn require_token(State(app): State<SharedState>, request: Request, next: Next) -> Response {
    if request_has_token(&app, &request) {
        next.run(request).await
    } else {
        error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ))
    }
}

async fn index(State(app): State<SharedState>) -> Html<String> {
    // The token doubles as the CSRF guard: a cross-site request cannot read
    // this page to obtain it.
    Html(INDEX_HTML.replace(
        "__CONTROL_PLANE_TOKEN_JSON__",
        &javascript_string_literal(&app.token),
    ))
}

fn javascript_string_literal(value: &str) -> String {
    serde_json::to_string(value)
        .expect("serializing a Rust string cannot fail")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

async fn app_js() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "application/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn styles_css() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/css; charset=utf-8")], STYLES_CSS)
}

async fn config_handler(State(app): State<SharedState>) -> Response {
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || config(&worker)).await {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

async fn live_handler() -> Response {
    Json(simchain_common::control_api::HealthResponse {
        status: "live".to_string(),
        ready: true,
    })
    .into_response()
}

async fn ready_handler(State(app): State<SharedState>) -> Response {
    let status = status(&app);
    let ready = status.last_updated_ms.is_some() && status.rpc_error.is_none();
    let body = Json(simchain_common::control_api::HealthResponse {
        status: if ready { "ready" } else { "not_ready" }.to_string(),
        ready,
    });
    if ready {
        body.into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    }
}

async fn status_handler(State(app): State<SharedState>) -> Response {
    Json(status(&app)).into_response()
}

#[derive(Clone, Debug, Default, Deserialize)]
struct DashboardQuery {
    #[serde(default)]
    tab: DashboardTab,
    #[serde(default)]
    selected_job_id: Option<String>,
    #[serde(default)]
    events_after: u64,
    #[serde(default = "default_dashboard_event_limit")]
    event_limit: usize,
    #[serde(default)]
    selected_faucet_txid: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum DashboardTab {
    #[default]
    All,
    Overview,
    Control,
    Faucet,
}

fn default_dashboard_event_limit() -> usize {
    200
}

#[derive(Clone, Debug, Serialize)]
struct DashboardResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    active_job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<StatusResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<ConfigResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    faucet: Option<FaucetStatusResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_faucet_transfer: Option<FaucetTransfer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_address: Option<UserAddressLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jobs: Option<JobListResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_job: Option<JobDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_job_events: Option<JobEventsResponse>,
}

#[derive(Clone, Debug, Serialize)]
struct UserAddressLink {
    address: String,
    explorer_url: String,
}

async fn dashboard_handler(
    State(app): State<SharedState>,
    query: Result<Query<DashboardQuery>, QueryRejection>,
) -> Response {
    let query = match query.map(|Query(query)| query) {
        Ok(query) => query,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid dashboard query: {rejection}"),
            ));
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || dashboard_snapshot(&worker, query)).await {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

fn dashboard_snapshot(
    app: &SharedState,
    query: DashboardQuery,
) -> Result<DashboardResponse, ServiceError> {
    let tab = query.tab;
    let active_job_id = app.jobs.active_summary().map(|job| job.id);
    let status = match tab {
        DashboardTab::All | DashboardTab::Overview | DashboardTab::Control => Some(status(app)),
        DashboardTab::Faucet => None,
    };
    let config = match tab {
        DashboardTab::All | DashboardTab::Control => Some(config(app)?),
        DashboardTab::Overview | DashboardTab::Faucet => None,
    };
    let faucet = match tab {
        DashboardTab::All | DashboardTab::Faucet => Some(faucet_status(app)),
        DashboardTab::Overview | DashboardTab::Control => None,
    };
    let jobs = match tab {
        DashboardTab::All | DashboardTab::Control => Some(list_jobs(app)),
        DashboardTab::Overview | DashboardTab::Faucet => None,
    };
    let selected_faucet_transfer = match (tab, query.selected_faucet_txid.as_deref()) {
        (DashboardTab::All | DashboardTab::Faucet, Some(txid)) => {
            match faucet_transfer(app, txid) {
                Ok(transfer) => Some(transfer),
                Err(error) if error.code == ErrorCode::JobNotFound => None,
                Err(error) => return Err(error),
            }
        }
        _ => None,
    };
    let user_address =
        matches!(tab, DashboardTab::All | DashboardTab::Faucet).then(|| UserAddressLink {
            address: app.config.user_address.clone(),
            explorer_url: format!(
                "{}/address/{}",
                app.config.explorer_url.trim_end_matches('/'),
                app.config.user_address
            ),
        });
    let include_selected_job = matches!(
        tab,
        DashboardTab::All | DashboardTab::Control | DashboardTab::Faucet
    );
    let selected_job_id = if include_selected_job {
        active_job_id
            .as_deref()
            .or(query.selected_job_id.as_deref())
            .or_else(|| {
                jobs.as_ref()
                    .and_then(|jobs| jobs.jobs.first().map(|job| job.id.as_str()))
            })
    } else {
        None
    };
    let selected_job = match selected_job_id {
        Some(job_id) => match get_job(app, job_id) {
            Ok(job) => Some(job),
            Err(error) if error.code == ErrorCode::JobNotFound => None,
            Err(error) => return Err(error),
        },
        None => None,
    };
    let selected_job_events = match (tab, selected_job.as_ref()) {
        (DashboardTab::All | DashboardTab::Control, Some(job)) => match job_events(
            app,
            Some(&job.summary.id),
            query.events_after,
            query.event_limit,
        ) {
            Ok(events) => Some(events),
            Err(error) if error.code == ErrorCode::JobNotFound => None,
            Err(error) => return Err(error),
        },
        _ => None,
    };
    Ok(DashboardResponse {
        active_job_id,
        status,
        config,
        faucet,
        selected_faucet_transfer,
        user_address,
        jobs,
        selected_job,
        selected_job_events,
    })
}

async fn schema_handler() -> Response {
    Json(schema()).into_response()
}

async fn config_patch_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let payload: Result<Json<ApplyRequest>, JsonRejection> = Json::from_request(request, &()).await;
    let Json(apply_request) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || apply(&worker, apply_request)).await {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

async fn mining_state_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let payload: Result<
        Json<simchain_common::control_api::SetComponentStateRequest>,
        JsonRejection,
    > = Json::from_request(request, &()).await;
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || set_mining_state(&worker, payload.state)).await {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

async fn spam_state_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let payload: Result<
        Json<simchain_common::control_api::SetComponentStateRequest>,
        JsonRejection,
    > = Json::from_request(request, &()).await;
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || set_spam_state(&worker, payload.state)).await {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: u64,
    #[serde(default = "default_event_limit")]
    limit: usize,
}

fn default_event_limit() -> usize {
    100
}

async fn global_events_handler(
    State(app): State<SharedState>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Response {
    let query = match events_query(query) {
        Ok(query) => query,
        Err(error) => return error_response(&error),
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || job_events(&worker, None, query.after, query.limit))
        .await
    {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

async fn jobs_handler(State(app): State<SharedState>) -> Response {
    Json(list_jobs(&app)).into_response()
}

async fn job_handler(
    State(app): State<SharedState>,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    match get_job(&app, &job_id) {
        Ok(job) => Json(job).into_response(),
        Err(error) => error_response(&error),
    }
}

async fn job_events_handler(
    State(app): State<SharedState>,
    AxumPath(job_id): AxumPath<String>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Response {
    let query = match events_query(query) {
        Ok(query) => query,
        Err(error) => return error_response(&error),
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || {
        job_events(&worker, Some(&job_id), query.after, query.limit)
    })
    .await
    {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

fn events_query(
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Result<EventsQuery, ServiceError> {
    query.map(|Query(query)| query).map_err(|rejection| {
        ServiceError::new(
            ErrorCode::ValidationFailed,
            format!("invalid event cursor query: {rejection}"),
        )
    })
}

async fn faucet_status_handler(State(app): State<SharedState>) -> Response {
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || faucet_status(&worker)).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

async fn faucet_transfer_handler(
    State(app): State<SharedState>,
    AxumPath(txid): AxumPath<String>,
) -> Response {
    match faucet_transfer(&app, &txid) {
        Ok(response) => Json(response).into_response(),
        Err(error) => error_response(&error),
    }
}

async fn faucet_job_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let idempotency_key = match request_idempotency_key(&request) {
        Ok(key) => key,
        Err(error) => return faucet_error_response(&error),
    };
    let payload: Result<Json<simchain_common::control_api::FaucetJobRequest>, JsonRejection> =
        Json::from_request(request, &()).await;
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return faucet_error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || start_faucet(&worker, payload, idempotency_key)).await
    {
        Ok(Ok(response)) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Ok(Err(error)) => faucet_error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

fn faucet_error_response(error: &ServiceError) -> Response {
    if error.code == ErrorCode::ValidationFailed {
        (StatusCode::BAD_REQUEST, Json(error.envelope())).into_response()
    } else {
        error_response(error)
    }
}

async fn reorg_job_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let idempotency_key = match request.headers().get("idempotency-key") {
        Some(value) => match value.to_str() {
            Ok(value) => Some(value.to_string()),
            Err(_) => {
                return error_response(&ServiceError::new(
                    ErrorCode::ValidationFailed,
                    "Idempotency-Key must be valid UTF-8",
                ));
            }
        },
        None => None,
    };
    let payload: Result<Json<simchain_common::control_api::ReorgJobRequest>, JsonRejection> =
        Json::from_request(request, &()).await;
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || start_reorg(&worker, payload, idempotency_key)).await
    {
        Ok(Ok(response)) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

async fn scenario_job_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let idempotency_key = match request.headers().get("idempotency-key") {
        Some(value) => match value.to_str() {
            Ok(value) => Some(value.to_string()),
            Err(_) => {
                return error_response(&ServiceError::new(
                    ErrorCode::ValidationFailed,
                    "Idempotency-Key must be valid UTF-8",
                ));
            }
        },
        None => None,
    };
    let is_json = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        });
    // JSON escaping can nearly double a valid 1 MiB YAML document. The job
    // manager enforces the decoded YAML limit for both submission forms.
    let body = match axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024 + 64 * 1024).await {
        Ok(body) => body,
        Err(error) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid scenario request body: {error}"),
            ));
        }
    };
    let yaml = if is_json {
        match serde_json::from_slice::<simchain_common::control_api::ScenarioJobRequest>(&body) {
            Ok(payload) => payload.yaml,
            Err(error) => {
                return error_response(&ServiceError::new(
                    ErrorCode::ValidationFailed,
                    format!("invalid scenario JSON envelope: {error}"),
                ));
            }
        }
    } else {
        match String::from_utf8(body.to_vec()) {
            Ok(yaml) => yaml,
            Err(error) => {
                return error_response(&ServiceError::new(
                    ErrorCode::ValidationFailed,
                    format!("scenario YAML must be UTF-8: {error}"),
                ));
            }
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || start_scenario(&worker, yaml, idempotency_key)).await
    {
        Ok(Ok(response)) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

async fn mine_job_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let idempotency_key = match request_idempotency_key(&request) {
        Ok(key) => key,
        Err(error) => return error_response(&error),
    };
    let payload: Result<Json<simchain_common::control_api::MineJobRequest>, JsonRejection> =
        Json::from_request(request, &()).await;
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || start_mine(&worker, payload, idempotency_key)).await {
        Ok(Ok(response)) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

async fn spam_burst_job_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let idempotency_key = match request_idempotency_key(&request) {
        Ok(key) => key,
        Err(error) => return error_response(&error),
    };
    let payload: Result<Json<simchain_common::control_api::SpamBurstJobRequest>, JsonRejection> =
        Json::from_request(request, &()).await;
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || start_spam_burst(&worker, payload, idempotency_key))
        .await
    {
        Ok(Ok(response)) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

async fn partition_job_handler(State(app): State<SharedState>, request: Request) -> Response {
    authenticated_job_request::<simchain_common::control_api::PartitionJobRequest, _>(
        app,
        request,
        start_partition,
    )
    .await
}

async fn degrade_job_handler(State(app): State<SharedState>, request: Request) -> Response {
    authenticated_job_request::<simchain_common::control_api::DegradeJobRequest, _>(
        app,
        request,
        start_degrade,
    )
    .await
}

async fn authenticated_job_request<T, F>(app: SharedState, request: Request, start: F) -> Response
where
    T: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(
            &SharedState,
            T,
            Option<String>,
        ) -> Result<simchain_common::control_api::JobCreatedResponse, ServiceError>
        + Send
        + Sync
        + Copy
        + 'static,
{
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let idempotency_key = match request_idempotency_key(&request) {
        Ok(key) => key,
        Err(error) => return error_response(&error),
    };
    let payload: Result<Json<T>, JsonRejection> = Json::from_request(request, &()).await;
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || start(&worker, payload, idempotency_key)).await {
        Ok(Ok(response)) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
}

fn request_idempotency_key(request: &Request) -> Result<Option<String>, ServiceError> {
    match request.headers().get("idempotency-key") {
        Some(value) => value
            .to_str()
            .map(|value| Some(value.to_string()))
            .map_err(|_| {
                ServiceError::new(
                    ErrorCode::ValidationFailed,
                    "Idempotency-Key must be valid UTF-8",
                )
            }),
        None => Ok(None),
    }
}

async fn checkpoint_handler(
    State(app): State<SharedState>,
    AxumPath((job_id, name)): AxumPath<(String, String)>,
) -> Response {
    match get_checkpoint(&app, &job_id, &name) {
        Ok(response) => Json(response).into_response(),
        Err(error) => error_response(&error),
    }
}

async fn release_checkpoint_handler(
    State(app): State<SharedState>,
    AxumPath((job_id, name)): AxumPath<(String, String)>,
    request: Request,
) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    let payload: Result<
        Json<simchain_common::control_api::ReleaseCheckpointRequest>,
        JsonRejection,
    > = Json::from_request(request, &()).await;
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    match release_checkpoint(&app, &job_id, &name, payload) {
        Ok(response) => Json(response).into_response(),
        Err(error) => error_response(&error),
    }
}

async fn abort_job_handler(
    State(app): State<SharedState>,
    AxumPath(job_id): AxumPath<String>,
    request: Request,
) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (set SIMCHAIN_CONTROL_TOKEN or use the local dev default)",
        ));
    }
    match abort_job(&app, &job_id) {
        Ok(response) => Json(response).into_response(),
        Err(error) => error_response(&error),
    }
}

// Json::from_request needs the trait in scope.
use axum::extract::FromRequest;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_app, MockBackend};
    use axum::body::Body;
    use axum::http::{header, Request as HttpRequest, StatusCode};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct Fixture {
        _dir: tempfile::TempDir,
        router: Router,
    }

    fn fixture(_boot_policy: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new());
        mock.sync_workers();
        let app = Arc::new(test_app(dir.path(), mock.clone()));
        Fixture {
            _dir: dir,
            router: router(app),
        }
    }

    async fn send(router: &Router, request: HttpRequest<Body>) -> (StatusCode, serde_json::Value) {
        let response = router.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    fn get(path: &str) -> HttpRequest<Body> {
        HttpRequest::get(path)
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("request")
    }

    fn patch_config(payload: serde_json::Value, token: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::patch("/api/v1/config")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder
            .body(Body::from(payload.to_string()))
            .expect("request")
    }

    fn put_mining_state(state: &str, token: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::put("/api/v1/mining/state")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder
            .body(Body::from(serde_json::json!({"state": state}).to_string()))
            .expect("request")
    }

    fn put_spam_state(state: &str, token: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::put("/api/v1/spam/state")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder
            .body(Body::from(serde_json::json!({"state": state}).to_string()))
            .expect("request")
    }

    fn post_reorg(
        payload: serde_json::Value,
        token: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> HttpRequest<Body> {
        let mut builder = HttpRequest::post("/api/v1/jobs/reorg")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(key) = idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }
        builder
            .body(Body::from(payload.to_string()))
            .expect("request")
    }

    fn post_faucet(
        payload: serde_json::Value,
        token: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> HttpRequest<Body> {
        let mut builder = HttpRequest::post("/api/v1/jobs/faucet")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(key) = idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }
        builder
            .body(Body::from(payload.to_string()))
            .expect("request")
    }

    fn post_abort(job_id: &str, token: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::post(format!("/api/v1/jobs/{job_id}/abort"))
            .header(header::HOST, "localhost");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::empty()).expect("request")
    }

    fn post_scenario(
        body: impl Into<Body>,
        content_type: &str,
        token: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> HttpRequest<Body> {
        let mut builder = HttpRequest::post("/api/v1/jobs/scenario")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, content_type);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(key) = idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }
        builder.body(body.into()).expect("request")
    }

    fn post_checkpoint_release(
        job_id: &str,
        name: &str,
        generation: u64,
        token: Option<&str>,
    ) -> HttpRequest<Body> {
        let mut builder =
            HttpRequest::post(format!("/api/v1/jobs/{job_id}/checkpoints/{name}/release"))
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder
            .body(Body::from(
                serde_json::json!({"generation": generation}).to_string(),
            ))
            .expect("request")
    }

    fn post_action(
        action: &str,
        payload: serde_json::Value,
        token: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> HttpRequest<Body> {
        let mut builder = HttpRequest::post(format!("/api/v1/jobs/{action}"))
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(key) = idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }
        builder
            .body(Body::from(payload.to_string()))
            .expect("request")
    }

    #[tokio::test]
    async fn versioned_read_routes_respond() {
        let fx = fixture(None);
        let (status, body) = send(&fx.router, get("/api/v1/config")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["generation"], 0);
        assert_eq!(body["desired"]["BLOCK_INTERVAL_MEAN_SECS"], "15");

        let (status, body) = send(&fx.router, get("/api/v1/config/schema")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["settings"].as_array().expect("settings").len(),
            simchain_common::live_tuning::MANAGED_SETTINGS.len()
        );
        assert!(!body["boot_settings"]
            .as_array()
            .expect("boot settings")
            .is_empty());

        let (status, body) = send(&fx.router, get("/health/live")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "live");
        let (status, body) = send(&fx.router, get("/health/ready")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ready"], false);

        let (status, _) = send(&fx.router, get("/api/v1/status")).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = send(&fx.router, get("/api/v1/dashboard")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["config"]["generation"], 0);
        assert!(body["jobs"]["jobs"].is_array());
        assert!(body["faucet"]["wallets"].is_array());
    }

    #[tokio::test]
    async fn dashboard_route_filters_payload_by_tab() {
        let fx = fixture(None);

        let (status, body) = send(&fx.router, get("/api/v1/dashboard?tab=overview")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["status"].is_object());
        assert!(body["config"].is_null());
        assert!(body["jobs"].is_null());
        assert!(body["faucet"].is_null());

        let (status, body) = send(&fx.router, get("/api/v1/dashboard?tab=control")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["status"].is_object());
        assert_eq!(body["config"]["generation"], 0);
        assert!(body["jobs"]["jobs"].is_array());
        assert!(body["faucet"].is_null());

        let (status, body) = send(&fx.router, get("/api/v1/dashboard?tab=faucet")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["faucet"]["wallets"].is_array());
        assert_eq!(
            body["user_address"]["address"],
            "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk"
        );
        assert_eq!(
            body["user_address"]["explorer_url"],
            "http://127.0.0.1:1080/address/bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk"
        );
        assert!(body["status"].is_null());
        assert!(body["config"].is_null());
        assert!(body["jobs"].is_null());
    }

    #[tokio::test]
    async fn faucet_route_requires_idempotency_and_uses_shared_transfer_reads() {
        let fx = fixture(None);
        let request = serde_json::json!({
            "source": "auto",
            "outputs": [{
                "address": "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr",
                "amount_sats": 100_000_000
            }]
        });
        let (status, body) = send(
            &fx.router,
            post_faucet(request.clone(), Some("test-token"), None),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "validation_failed");

        let (status, body) = send(
            &fx.router,
            post_faucet(request.clone(), Some("test-token"), Some("faucet-retry")),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let job_id = body["job_id"].as_str().unwrap().to_string();
        let job = loop {
            let (status, body) = send(&fx.router, get(&format!("/api/v1/jobs/{job_id}"))).await;
            assert_eq!(status, StatusCode::OK);
            if matches!(body["state"].as_str(), Some("succeeded" | "failed")) {
                break body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        assert_eq!(job["state"], "succeeded", "{job}");
        assert_eq!(job["result"]["actual_fee_sats"], 0);
        assert_eq!(job["result"]["priority_delta_sats"], 10_000_000_000_i64);
        assert!(!job.to_string().contains("raw_tx_hex"));
        let txid = job["result"]["txid"].as_str().unwrap();

        let (status, transfer) =
            send(&fx.router, get(&format!("/api/v1/faucet/transfers/{txid}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(transfer["delivery_state"], "armed");
        assert_eq!(transfer["actual_fee_sats"], 0);
        assert!(!transfer.to_string().contains("raw_tx_hex"));

        let (status, faucet) = send(&fx.router, get("/api/v1/faucet")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(faucet["pending_transfer"]["txid"], txid);

        let (status, reused) = send(
            &fx.router,
            post_faucet(request, Some("test-token"), Some("faucet-retry")),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(reused["job_id"], job_id);
        assert_eq!(reused["reused"], true);
    }

    #[tokio::test]
    async fn unknown_api_path_is_404() {
        let fx = fixture(None);
        let (status, _) = send(&fx.router, get("/api/v1/nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn removed_transitional_config_routes_stay_absent() {
        let fx = fixture(None);
        for request in [
            get("/api/v1/state"),
            get("/api/v1/schema"),
            HttpRequest::post("/api/v1/apply")
                .header(header::HOST, "localhost")
                .body(Body::empty())
                .expect("request"),
        ] {
            let (status, _) = send(&fx.router, request).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn config_patch_without_token_is_unauthorized() {
        let fx = fixture(None);
        let payload = serde_json::json!({"settings": {"SPAM_FEE": "0.0002"}});
        let (status, body) = send(&fx.router, patch_config(payload.clone(), None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
        let (status, body) = send(&fx.router, patch_config(payload, Some("wrong"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
        let (_, config) = send(&fx.router, get("/api/v1/config")).await;
        assert_eq!(config["generation"], 0);
    }

    #[tokio::test]
    async fn config_patch_merges_partially_into_durable_state() {
        let fx = fixture(None);
        let payload = serde_json::json!({"settings": {"MINER_WEIGHTS": "70,30"}});
        let (status, body) = send(&fx.router, patch_config(payload, Some("test-token"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["changed"], true);
        assert_eq!(
            body["components_applied"],
            serde_json::json!(["mining", "spam"])
        );
        let (_, config) = send(&fx.router, get("/api/v1/config")).await;
        assert_eq!(config["desired"]["MINER_WEIGHTS"], "70,30");
        assert_eq!(config["desired"]["SPAM_FEE"], "0.0001");
    }

    #[tokio::test]
    async fn stale_generation_yields_409_with_code() {
        let fx = fixture(None);
        let payload = serde_json::json!({
            "settings": {"SPAM_FEE": "0.0003"},
            "base_generation": 99
        });
        let (status, body) = send(&fx.router, patch_config(payload, Some("test-token"))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "stale_revision");
    }

    #[tokio::test]
    async fn config_patch_uses_token_and_generation_cas() {
        let fx = fixture(None);
        let payload = serde_json::json!({
            "settings": {"BLOCK_INTERVAL_MEAN_SECS": "12"},
            "base_generation": 0
        });
        let (status, body) = send(&fx.router, patch_config(payload.clone(), None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");

        let (status, body) = send(&fx.router, patch_config(payload, Some("test-token"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["generation"], 1);

        let stale = serde_json::json!({
            "settings": {"BLOCK_INTERVAL_MEAN_SECS": "13"},
            "base_generation": 0
        });
        let (status, body) = send(&fx.router, patch_config(stale, Some("test-token"))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "stale_revision");
    }

    #[tokio::test]
    async fn mining_pause_uses_the_worker_api() {
        let fx = fixture(None);
        let (status, body) = send(&fx.router, put_mining_state("paused", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");

        let (status, body) = send(&fx.router, put_mining_state("paused", Some("test-token"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["component"], "mining");
        assert_eq!(body["desired_state"], "paused");
        assert_eq!(body["effective_state"], "paused");
    }

    #[tokio::test]
    async fn spam_pause_uses_the_worker_api() {
        let fx = fixture(None);
        let (status, body) = send(&fx.router, put_spam_state("paused", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");

        let (status, body) = send(&fx.router, put_spam_state("paused", Some("test-token"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["component"], "spam");
        assert_eq!(body["desired_state"], "paused");
        assert_eq!(body["effective_state"], "paused");
    }

    #[tokio::test]
    async fn reorg_jobs_are_authenticated_idempotent_and_queryable() {
        let fx = fixture(None);
        let request = serde_json::json!({
            "depth": 3,
            "empty": true,
            "node": "node3",
            "adds_new_txs": 0,
            "double_spend_pct": 0
        });
        let (status, body) = send(&fx.router, post_reorg(request.clone(), None, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");

        let (status, body) = send(
            &fx.router,
            post_reorg(request.clone(), Some("test-token"), Some("reorg-retry")),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let job_id = body["job_id"].as_str().expect("job ID").to_string();
        assert_eq!(body["reused"], serde_json::Value::Null);

        let (status, body) = send(
            &fx.router,
            post_reorg(request, Some("test-token"), Some("reorg-retry")),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["job_id"], job_id);
        assert_eq!(body["reused"], true);

        let (status, body) = send(&fx.router, get("/api/v1/jobs")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["jobs"][0]["id"], job_id);

        let (status, body) = send(&fx.router, get(&format!("/api/v1/jobs/{job_id}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], job_id);
        assert_eq!(body["request"]["empty"], true);

        let (status, body) = send(
            &fx.router,
            get(&format!("/api/v1/jobs/{job_id}/events?after=0&limit=20")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body["events"].as_array().expect("events").is_empty());

        let (status, body) = send(&fx.router, get("/api/v1/events?after=0&limit=20")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["events"]
            .as_array()
            .expect("global events")
            .iter()
            .all(|event| event["job_id"] == job_id));
        let (status, body) = send(&fx.router, get("/api/v1/events?after=not-a-number")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "validation_failed");

        let (status, body) = send(&fx.router, post_abort(&job_id, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
        let (status, body) = send(&fx.router, post_abort(&job_id, Some("test-token"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["job_id"], job_id);

        let (status, body) = send(&fx.router, get("/api/v1/jobs/missing")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "job_not_found");
    }

    #[tokio::test]
    async fn scenario_yaml_and_json_envelopes_share_checkpoint_contract() {
        let fx = fixture(None);
        let yaml = r#"
version: 1
steps:
  - type: pause_mining
  - type: checkpoint
    name: ci_hold
    timeout_secs: 5
  - type: resume_mining
"#;
        let (status, body) = send(
            &fx.router,
            post_scenario(yaml, "application/yaml", None, None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");

        let (status, body) = send(
            &fx.router,
            post_scenario(
                yaml,
                "application/yaml",
                Some("test-token"),
                Some("scenario-http-retry"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let job_id = body["job_id"].as_str().expect("job ID").to_string();

        let envelope = serde_json::json!({"yaml": yaml}).to_string();
        let (status, body) = send(
            &fx.router,
            post_scenario(
                envelope,
                "application/json; charset=utf-8",
                Some("test-token"),
                Some("scenario-http-retry"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["job_id"], job_id);
        assert_eq!(body["reused"], true);

        let checkpoint_path = format!("/api/v1/jobs/{job_id}/checkpoints/ci_hold");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let checkpoint = loop {
            let (status, body) = send(&fx.router, get(&checkpoint_path)).await;
            assert_eq!(status, StatusCode::OK);
            if body["checkpoint"]["state"] == "reached" {
                break body;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "checkpoint was not reached"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        let generation = checkpoint["checkpoint"]["generation"]
            .as_u64()
            .expect("generation");
        assert!(checkpoint["checkpoint"]["live_summary"].is_object());

        let (status, body) = send(
            &fx.router,
            post_checkpoint_release(&job_id, "ci_hold", generation, None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
        let (status, body) = send(
            &fx.router,
            post_checkpoint_release(&job_id, "ci_hold", generation + 1, Some("test-token")),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "checkpoint_conflict");

        for _ in 0..2 {
            let (status, body) = send(
                &fx.router,
                post_checkpoint_release(&job_id, "ci_hold", generation, Some("test-token")),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["checkpoint"]["state"], "released");
        }

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let (status, body) = send(&fx.router, get(&format!("/api/v1/jobs/{job_id}"))).await;
            assert_eq!(status, StatusCode::OK);
            if body["state"] == "succeeded" {
                assert!(body["result"].is_object());
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "scenario did not finish"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn invalid_scenario_is_rejected_before_reserving_the_coordinator() {
        let fx = fixture(None);
        let invalid = "version: 1\nsteps:\n  - type: checkpoint\n    name: held\n";
        let (status, body) = send(
            &fx.router,
            post_scenario(invalid, "text/yaml", Some("test-token"), None),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "validation_failed");
        let (status, body) = send(&fx.router, get("/api/v1/jobs")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["jobs"].as_array().expect("jobs").is_empty());
        assert!(body["active_job_id"].is_null());
    }

    #[tokio::test]
    async fn manual_mine_and_spam_burst_use_dedicated_job_contracts() {
        let fx = fixture(None);
        let (status, body) = send(
            &fx.router,
            post_action(
                "mine",
                serde_json::json!({"node": "node2", "blocks": 2}),
                None,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");

        let (status, body) = send(
            &fx.router,
            post_action(
                "mine",
                serde_json::json!({"node": "node2", "blocks": 2}),
                Some("test-token"),
                Some("mine-http"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let mine_id = body["job_id"].as_str().expect("mine ID").to_string();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let (_, job) = send(&fx.router, get(&format!("/api/v1/jobs/{mine_id}"))).await;
            if job["state"] == "succeeded" {
                assert_eq!(job["kind"], "mine");
                assert_eq!(job["result"]["blocks"], 2);
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let (status, body) = send(
            &fx.router,
            post_action(
                "spam-burst",
                serde_json::json!({"node": "node3", "txs": 3, "data_bytes": 512}),
                Some("test-token"),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let burst_id = body["job_id"].as_str().expect("burst ID").to_string();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let (_, job) = send(&fx.router, get(&format!("/api/v1/jobs/{burst_id}"))).await;
            if job["state"] == "succeeded" {
                assert_eq!(job["kind"], "spam_burst");
                assert_eq!(job["result"]["accepted_transactions"], 3);
                assert_eq!(job["result"]["data_bytes"], 512);
                assert_eq!(job["result"]["shape"], "op_return");
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn partition_and_degrade_routes_share_the_job_contract() {
        let fx = fixture(None);
        let partition = serde_json::json!({
            "node": "node3",
            "main_blocks": 2,
            "isolated_blocks": 3
        });
        let (status, body) = send(
            &fx.router,
            post_action("partition", partition.clone(), None, None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");

        let (status, body) = send(
            &fx.router,
            post_action(
                "partition",
                partition,
                Some("test-token"),
                Some("partition-http"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let job_id = body["job_id"].as_str().expect("job ID").to_string();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let (_, job) = send(&fx.router, get(&format!("/api/v1/jobs/{job_id}"))).await;
            if job["state"] == "succeeded" {
                assert_eq!(job["kind"], "partition");
                assert_eq!(job["result"]["expected_tip"], "node3-3");
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let (status, body) = send(
            &fx.router,
            post_action(
                "degrade",
                serde_json::json!({
                    "node": "node1",
                    "delay_ms": 0,
                    "loss_pct": 0,
                    "seconds": 10
                }),
                Some("test-token"),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "validation_failed");
    }

    #[tokio::test]
    async fn invalid_value_yields_validation_failed() {
        let fx = fixture(None);
        let payload = serde_json::json!({"settings": {"ENABLE_SPAM": "maybe"}});
        let (status, body) = send(&fx.router, patch_config(payload, Some("test-token"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "validation_failed");
        assert_eq!(body["error"]["details"][0]["key"], "ENABLE_SPAM");
    }

    #[tokio::test]
    async fn malformed_body_uses_the_error_envelope() {
        let fx = fixture(None);
        let request = HttpRequest::patch("/api/v1/config")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, "Bearer test-token")
            .body(Body::from("{not json"))
            .expect("request");
        let (status, body) = send(&fx.router, request).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "validation_failed");
    }

    #[tokio::test]
    async fn index_injects_the_token_for_the_browser() {
        let fx = fixture(None);
        let response = fx.router.clone().oneshot(get("/")).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("test-token"));
        assert!(!html.contains("__CONTROL_PLANE_TOKEN_JSON__"));
    }

    #[test]
    fn token_literal_cannot_break_out_of_the_script_element() {
        let literal = javascript_string_literal("</script><script>alert(1)</script>&");
        assert!(!literal.contains('<'));
        assert!(!literal.contains('>'));
        assert!(!literal.contains('&'));
        assert!(literal.starts_with('"') && literal.ends_with('"'));
    }

    #[tokio::test]
    async fn non_loopback_host_cannot_read_the_page_or_token() {
        let fx = fixture(None);
        let request = HttpRequest::get("/")
            .header(header::HOST, "attacker.example:8090")
            .body(Body::empty())
            .expect("request");
        let response = fx.router.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert!(!String::from_utf8_lossy(&bytes).contains("test-token"));
    }

    #[tokio::test]
    async fn ipv6_loopback_host_is_allowed() {
        let fx = fixture(None);
        let request = HttpRequest::get("/api/v1/status")
            .header(header::HOST, "[::1]:8090")
            .body(Body::empty())
            .expect("request");
        let response = fx.router.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn mcp_endpoint_requires_the_token() {
        let fx = fixture(None);
        let request = HttpRequest::post("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            ))
            .expect("request");
        let (status, body) = send(&fx.router, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn mcp_lists_control_plane_tools() {
        let router = crate::mcp::ControlPlaneMcp::tool_router();
        let mut names: Vec<String> = router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "abort_job",
                "fund_addresses",
                "get_config",
                "get_config_schema",
                "get_faucet_status",
                "get_faucet_transfer",
                "get_job",
                "get_status",
                "list_jobs",
                "release_checkpoint",
                "set_config",
                "set_mining_state",
                "set_spam_state",
                "start_degrade",
                "start_partition",
                "start_reorg",
                "start_scenario",
            ]
        );
    }

    #[tokio::test]
    async fn mcp_reorg_uses_the_same_job_service_contract() {
        use rmcp::handler::server::wrapper::Parameters;
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new());
        mock.sync_workers();
        let app = Arc::new(test_app(dir.path(), mock));
        let mcp = crate::mcp::ControlPlaneMcp::new(app);

        let result = mcp
            .start_reorg(Parameters(crate::mcp::StartReorgParams {
                depth: 2,
                empty: true,
                node: "node3".to_string(),
                adds_new_txs: 0,
                double_spend_pct: 0,
                idempotency_key: Some("mcp-reorg".to_string()),
            }))
            .await
            .expect("tool result");
        assert_ne!(result.is_error, Some(true));
        let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        let created: serde_json::Value = serde_json::from_str(&text.text).expect("job JSON");
        let job_id = created["job_id"].as_str().expect("job ID").to_string();

        let result = mcp
            .get_job(Parameters(crate::mcp::JobIdParams { job_id }))
            .await
            .expect("tool result");
        assert_ne!(result.is_error, Some(true));
        let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        let job: serde_json::Value = serde_json::from_str(&text.text).expect("job JSON");
        assert_eq!(job["kind"], "reorg");
        assert_eq!(job["request"]["depth"], 2);
    }

    #[tokio::test]
    async fn mcp_scenario_and_checkpoint_use_the_shared_job_contract() {
        use rmcp::handler::server::wrapper::Parameters;
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new());
        mock.sync_workers();
        let app = Arc::new(test_app(dir.path(), mock));
        let mcp = crate::mcp::ControlPlaneMcp::new(app.clone());
        let result = mcp
            .start_scenario(Parameters(crate::mcp::StartScenarioParams {
                yaml: r#"
version: 1
steps:
  - type: checkpoint
    name: mcp_hold
    timeout_secs: 5
"#
                .to_string(),
                idempotency_key: Some("mcp-scenario".to_string()),
            }))
            .await
            .expect("tool result");
        assert_ne!(result.is_error, Some(true));
        let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        let created: serde_json::Value = serde_json::from_str(&text.text).expect("job JSON");
        let job_id = created["job_id"].as_str().expect("job ID").to_string();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let generation = loop {
            if let Ok(response) = app.jobs.checkpoint(&job_id, "mcp_hold") {
                if response.checkpoint.state
                    == simchain_common::control_api::CheckpointState::Reached
                {
                    break response.checkpoint.generation;
                }
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        let result = mcp
            .release_checkpoint(Parameters(crate::mcp::ReleaseCheckpointParams {
                job_id: job_id.clone(),
                checkpoint: "mcp_hold".to_string(),
                generation,
            }))
            .await
            .expect("release tool result");
        assert_ne!(result.is_error, Some(true));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let job = app.jobs.get(&job_id).expect("job");
            if job.summary.state.is_terminal() {
                assert_eq!(
                    job.summary.state,
                    simchain_common::control_api::JobState::Succeeded
                );
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn mcp_apply_settings_rejects_invalid_values_without_side_effects() {
        use rmcp::handler::server::wrapper::Parameters;
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new());
        mock.sync_workers();
        let app = Arc::new(test_app(dir.path(), mock));
        let mcp = crate::mcp::ControlPlaneMcp::new(app);

        let params = crate::mcp::SetConfigParams {
            settings: [("MINER_WEIGHTS".to_string(), "0,0".to_string())]
                .into_iter()
                .collect(),
            base_generation: None,
        };
        let result = mcp
            .set_config(Parameters(params))
            .await
            .expect("tool call returns a result");
        assert_eq!(result.is_error, Some(true));
        let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        let body: serde_json::Value = serde_json::from_str(&text.text).expect("json envelope");
        assert_eq!(body["error"]["code"], "validation_failed");
    }

    #[tokio::test]
    async fn mcp_get_settings_matches_the_http_state_payload() {
        let fx = fixture(None);
        let (_, http_body) = send(&fx.router, get("/api/v1/config")).await;

        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockBackend::new());
        mock.sync_workers();
        let app = Arc::new(test_app(dir.path(), mock));
        let result = crate::mcp::ControlPlaneMcp::new(app)
            .get_config()
            .await
            .expect("tool result");
        let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        let mcp_body: serde_json::Value = serde_json::from_str(&text.text).expect("json");
        // HTTP and MCP are thin adapters over the same desired/effective service.
        assert_eq!(mcp_body["desired"], http_body["desired"]);
        assert_eq!(mcp_body["generation"], http_body["generation"]);
        assert_eq!(mcp_body["pending_apply"], http_body["pending_apply"]);
    }
}
