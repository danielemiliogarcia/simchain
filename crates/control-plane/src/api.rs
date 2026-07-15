//! HTTP surface: versioned JSON API (v1), token auth on mutations, the
//! embedded browser UI, and the `/mcp` mount. Thin adapter over `service`.

use crate::apply::{apply, ApplyRequest};
use crate::service::{
    config, schema, set_mining_state, settings_state, status, ErrorCode, ServiceError,
};
use crate::state::SharedState;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use axum::http::uri::Authority;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
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
        .route("/api/v1/mining/state", put(mining_state_handler))
        // Phase-1 compatibility routes; removed with the Compose adapter.
        .route("/api/v1/state", get(state_handler))
        .route("/api/v1/status", get(status_handler))
        .route("/api/v1/schema", get(schema_handler))
        .route("/api/v1/apply", post(apply_handler))
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
            "missing or invalid bearer token (see .simchain-control/token)",
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

async fn state_handler(State(app): State<SharedState>) -> Response {
    let worker = app.clone();
    match tokio::task::spawn_blocking(move || settings_state(&worker)).await {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(error) => error_response(&ServiceError::new(
            ErrorCode::Internal,
            format!("control-plane worker task failed: {error}"),
        )),
    }
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

async fn schema_handler() -> Response {
    Json(schema()).into_response()
}

async fn apply_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (see .simchain-control/token)",
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

async fn config_patch_handler(State(app): State<SharedState>, request: Request) -> Response {
    if !request_has_token(&app, &request) {
        return error_response(&ServiceError::new(
            ErrorCode::Unauthorized,
            "missing or invalid bearer token (see .simchain-control/token)",
        ));
    }
    let payload: Result<Json<ApplyRequest>, JsonRejection> = Json::from_request(request, &()).await;
    let Json(mut apply_request) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            return error_response(&ServiceError::new(
                ErrorCode::ValidationFailed,
                format!("invalid request body: {rejection}"),
            ));
        }
    };
    // The public config contract uses generations. The env revision remains
    // internal to the transitional adapter.
    apply_request.base_revision = None;
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
            "missing or invalid bearer token (see .simchain-control/token)",
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
        env_file: std::path::PathBuf,
        mock: Arc<MockBackend>,
    }

    fn fixture(env_content: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        if let Some(content) = env_content {
            std::fs::write(&env_file, content).expect("seed env");
        }
        let mock = Arc::new(MockBackend::new(env_file.clone()));
        mock.sync_containers();
        let app = Arc::new(test_app(dir.path(), mock.clone()));
        Fixture {
            _dir: dir,
            router: router(app),
            env_file,
            mock,
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

    fn post_apply(payload: serde_json::Value, token: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::post("/api/v1/apply")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder
            .body(Body::from(payload.to_string()))
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

    #[tokio::test]
    async fn versioned_read_routes_respond() {
        let fx = fixture(None);
        let (status, body) = send(&fx.router, get("/api/v1/config")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["generation"], 0);
        assert_eq!(body["desired"]["BLOCK_INTERVAL_MEAN_SECS"], "15");

        let (status, body) = send(&fx.router, get("/api/v1/config/schema")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["settings"].as_array().expect("settings").len() >= 20);

        let (status, body) = send(&fx.router, get("/health/live")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "live");
        let (status, body) = send(&fx.router, get("/health/ready")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ready"], false);

        let (status, _) = send(&fx.router, get("/api/v1/status")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_api_path_is_404() {
        let fx = fixture(None);
        let (status, _) = send(&fx.router, get("/api/v1/nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn apply_without_token_is_unauthorized() {
        let fx = fixture(None);
        let payload = serde_json::json!({"settings": {"FALLBACK_FEE": "0.0002"}});
        let (status, body) = send(&fx.router, post_apply(payload.clone(), None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
        let (status, body) = send(&fx.router, post_apply(payload, Some("wrong"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
        assert!(!fx.env_file.exists(), "no side effects without the token");
    }

    #[tokio::test]
    async fn apply_with_token_merges_partially() {
        let fx = fixture(Some("FALLBACK_FEE=0.0002\n"));
        let payload = serde_json::json!({"settings": {"MINER_WEIGHTS": "70,30"}});
        let (status, body) = send(&fx.router, post_apply(payload, Some("test-token"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["changed"], true);
        assert_eq!(
            body["components_applied"][0],
            "btc-simnet-mining-controller"
        );
        let written = std::fs::read_to_string(&fx.env_file).expect("read");
        assert!(
            written.contains("FALLBACK_FEE=0.0002"),
            "omitted keys untouched"
        );
        assert!(written.contains("MINER_WEIGHTS=70,30"));
    }

    #[tokio::test]
    async fn stale_revision_yields_409_with_code() {
        let fx = fixture(Some("FALLBACK_FEE=0.0002\n"));
        let payload = serde_json::json!({
            "settings": {"FALLBACK_FEE": "0.0003"},
            "base_revision": "deadbeef"
        });
        let (status, body) = send(&fx.router, post_apply(payload, Some("test-token"))).await;
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
    async fn mining_pause_uses_worker_api_and_never_compose() {
        let fx = fixture(None);
        let (status, body) = send(&fx.router, put_mining_state("paused", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");

        let (status, body) = send(&fx.router, put_mining_state("paused", Some("test-token"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["component"], "mining");
        assert_eq!(body["desired_state"], "paused");
        assert_eq!(body["effective_state"], "paused");
        assert!(fx.mock.compose_calls().is_empty());
    }

    #[tokio::test]
    async fn invalid_value_yields_validation_failed() {
        let fx = fixture(None);
        let payload = serde_json::json!({"settings": {"ENABLE_SPAM": "maybe"}});
        let (status, body) = send(&fx.router, post_apply(payload, Some("test-token"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "validation_failed");
        assert_eq!(body["error"]["details"][0]["key"], "ENABLE_SPAM");
    }

    #[tokio::test]
    async fn malformed_body_uses_the_error_envelope() {
        let fx = fixture(None);
        let request = HttpRequest::post("/api/v1/apply")
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
    async fn mcp_lists_phase_two_tools() {
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
                "get_config",
                "get_config_schema",
                "get_status",
                "set_config",
                "set_mining_state",
            ]
        );
    }

    #[tokio::test]
    async fn mcp_apply_settings_rejects_invalid_values_without_side_effects() {
        use rmcp::handler::server::wrapper::Parameters;
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        let mock = Arc::new(MockBackend::new(env_file.clone()));
        mock.sync_containers();
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
        assert!(!env_file.exists(), ".env must be untouched");
    }

    #[tokio::test]
    async fn mcp_get_settings_matches_the_http_state_payload() {
        let fx = fixture(Some("FALLBACK_FEE=0.0002\n"));
        let (_, http_body) = send(&fx.router, get("/api/v1/config")).await;

        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "FALLBACK_FEE=0.0002\n").expect("seed env");
        let mock = Arc::new(MockBackend::new(env_file));
        mock.sync_containers();
        let app = Arc::new(test_app(dir.path(), mock));
        let result = crate::mcp::ControlPlaneMcp::new(app)
            .get_config()
            .await
            .expect("tool result");
        let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        let mcp_body: serde_json::Value = serde_json::from_str(&text.text).expect("json");
        // Same shape and same staged values (revisions match too: same content).
        assert_eq!(mcp_body["desired"], http_body["desired"]);
        assert_eq!(mcp_body["generation"], http_body["generation"]);
        assert_eq!(mcp_body["pending_apply"], http_body["pending_apply"]);
    }
}
