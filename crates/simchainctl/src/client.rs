use serde::de::DeserializeOwned;
use simchain_common::control_api::{ApiErrorEnvelope, ConfigResponse, StatusResponse, API_PREFIX};
use std::fmt;

#[derive(Debug)]
pub enum ClientError {
    Unavailable(String),
    Authentication(String),
    Api(String),
    Decode(String),
    Output(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message)
            | Self::Authentication(message)
            | Self::Api(message)
            | Self::Decode(message)
            | Self::Output(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ClientError {}

pub struct ControlClient {
    base_url: String,
    token: Option<String>,
}

impl ControlClient {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        }
    }

    pub fn status(&self) -> Result<StatusResponse, ClientError> {
        self.get(&format!("{API_PREFIX}/status"))
    }

    pub fn config(&self) -> Result<ConfigResponse, ClientError> {
        self.get(&format!("{API_PREFIX}/config"))
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let url = format!("{}{path}", self.base_url);
        let mut request = minreq::get(&url).with_timeout(10);
        if let Some(token) = self.token.as_deref() {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        let response = request
            .send()
            .map_err(|error| ClientError::Unavailable(format!("cannot reach {url}: {error}")))?;
        let body = response.as_str().map_err(|error| {
            ClientError::Decode(format!("invalid response from {url}: {error}"))
        })?;
        if !(200..300).contains(&response.status_code) {
            let message = serde_json::from_str::<ApiErrorEnvelope>(body)
                .map(|envelope| envelope.error.message)
                .unwrap_or_else(|_| format!("HTTP {}: {body}", response.status_code));
            return if response.status_code == 401 || response.status_code == 403 {
                Err(ClientError::Authentication(message))
            } else if response.status_code >= 500 {
                Err(ClientError::Unavailable(message))
            } else {
                Err(ClientError::Api(message))
            };
        }
        serde_json::from_str(body)
            .map_err(|error| ClientError::Decode(format!("invalid JSON from {url}: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn status_uses_the_versioned_contract_and_deserializes_shared_dto() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 2048];
            let read = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /api/v1/status HTTP/1.1"));

            let status = StatusResponse {
                height: Some(204),
                ..StatusResponse::default()
            };
            let body = serde_json::to_string(&status).expect("status JSON");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("response");
        });

        let client = ControlClient::new(format!("http://{address}"), None);
        let status = client.status().expect("status response");
        assert_eq!(status.height, Some(204));
        server.join().expect("server");
    }
}
