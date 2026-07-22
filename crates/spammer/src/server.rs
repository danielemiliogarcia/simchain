//! Small synchronous authenticated HTTP server for the spam worker.

use crate::control::SpamControl;
use simchain_common::control_api::{ApiError, ErrorCode};
use simchain_common::internal_api::{
    LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest, SetSpamPolicyRequest, SetStateRequest,
    INTERNAL_API_PREFIX,
};
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const MAX_BODY_BYTES: u64 = 1024 * 1024;

pub fn spawn(
    listen_addr: SocketAddr,
    token: String,
    control: Arc<SpamControl>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let server = Server::http(listen_addr)
        .map_err(|error| anyhow::anyhow!("bind spam control server: {error}"))?;
    tracing::info!(%listen_addr, "spam internal control API listening");
    Ok(std::thread::spawn(move || {
        for request in server.incoming_requests() {
            if let Err(error) = handle_request(request, &token, &control) {
                tracing::warn!("spam control request failed: {error}");
            }
        }
    }))
}

fn handle_request(mut request: Request, token: &str, control: &SpamControl) -> anyhow::Result<()> {
    if !authorized(&request, token) {
        return respond_error(
            request,
            ApiError::new(
                ErrorCode::Unauthorized,
                "missing or invalid internal bearer token",
            ),
        );
    }
    let method = request.method().clone();
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or(request.url())
        .to_string();
    match (method, path.as_str()) {
        (Method::Get, path) if path == format!("{INTERNAL_API_PREFIX}/status") => {
            respond_json(request, 200, &control.status())
        }
        (Method::Put, path) if path == format!("{INTERNAL_API_PREFIX}/state") => {
            match read_json::<SetStateRequest>(&mut request) {
                Ok(body) => match control.set_state(body) {
                    Ok(ack) => respond_json(request, 200, &ack),
                    Err(error) => respond_domain_error(request, error),
                },
                Err(error) => respond_body_error(request, error),
            }
        }
        (Method::Put, path) if path == format!("{INTERNAL_API_PREFIX}/config") => {
            match read_json::<SetSpamPolicyRequest>(&mut request) {
                Ok(body) => match control.set_policy(body) {
                    Ok(ack) => respond_json(request, 200, &ack),
                    Err(error) => respond_domain_error(request, error),
                },
                Err(error) => respond_body_error(request, error),
            }
        }
        (Method::Post, path) if path == format!("{INTERNAL_API_PREFIX}/leases") => {
            match read_json::<LeaseRequest>(&mut request) {
                Ok(body) => match control.acquire_lease(body) {
                    Ok(ack) => respond_json(request, 200, &ack),
                    Err(error) => respond_domain_error(request, error),
                },
                Err(error) => respond_body_error(request, error),
            }
        }
        (Method::Post, path) if lease_path(path, "/renew").is_some() => {
            let lease_id = lease_path(path, "/renew")
                .expect("checked lease path")
                .to_string();
            match read_json::<LeaseRenewRequest>(&mut request) {
                Ok(body) => match control.renew_lease(&lease_id, body) {
                    Ok(ack) => respond_json(request, 200, &ack),
                    Err(error) => respond_domain_error(request, error),
                },
                Err(error) => respond_body_error(request, error),
            }
        }
        (Method::Delete, path) if lease_path(path, "").is_some() => {
            let lease_id = lease_path(path, "")
                .expect("checked lease path")
                .to_string();
            match read_json::<LeaseReleaseRequest>(&mut request) {
                Ok(body) => match control.release_lease(&lease_id, body) {
                    Ok(ack) => respond_json(request, 200, &ack),
                    Err(error) => respond_domain_error(request, error),
                },
                Err(error) => respond_body_error(request, error),
            }
        }
        _ => respond_error(
            request,
            ApiError::new(ErrorCode::JobNotFound, "internal endpoint not found"),
        ),
    }
}

fn authorized(request: &Request, expected: &str) -> bool {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .and_then(|header| header.value.as_str().strip_prefix("Bearer "))
        .is_some_and(|provided| token_matches(expected, provided))
}

fn token_matches(expected: &str, provided: &str) -> bool {
    if expected.len() != provided.len() {
        return false;
    }
    expected
        .as_bytes()
        .iter()
        .zip(provided.as_bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn read_json<T: serde::de::DeserializeOwned>(request: &mut Request) -> anyhow::Result<T> {
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_string(&mut body)?;
    serde_json::from_str(&body).map_err(Into::into)
}

fn lease_path<'a>(path: &'a str, suffix: &str) -> Option<&'a str> {
    let prefix = format!("{INTERNAL_API_PREFIX}/leases/");
    let lease_id = path.strip_prefix(&prefix)?.strip_suffix(suffix)?;
    (!lease_id.is_empty() && !lease_id.contains('/')).then_some(lease_id)
}

fn respond_domain_error(request: Request, error: anyhow::Error) -> anyhow::Result<()> {
    respond_error(
        request,
        ApiError::new(ErrorCode::ValidationFailed, error.to_string()),
    )
}

fn respond_body_error(request: Request, error: anyhow::Error) -> anyhow::Result<()> {
    respond_error(
        request,
        ApiError::new(
            ErrorCode::ValidationFailed,
            format!("invalid request body: {error}"),
        ),
    )
}

fn respond_error(request: Request, error: ApiError) -> anyhow::Result<()> {
    respond_json(request, error.code.http_status(), &error.envelope())
}

fn respond_json(
    request: Request,
    status: u16,
    value: &impl serde::Serialize,
) -> anyhow::Result<()> {
    let body = serde_json::to_string(value)?;
    let response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(
            Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                .map_err(|_| anyhow::anyhow!("invalid content-type header"))?,
        );
    request.respond(response)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_route_parser_is_strict() {
        assert_eq!(
            lease_path("/internal/v1/leases/abc/renew", "/renew"),
            Some("abc")
        );
        assert_eq!(lease_path("/internal/v1/leases/abc", ""), Some("abc"));
        assert_eq!(lease_path("/internal/v1/leases/a/b", ""), None);
    }

    #[test]
    fn token_comparison_requires_exact_value() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "secrex"));
        assert!(!token_matches("secret", "short"));
    }
}
