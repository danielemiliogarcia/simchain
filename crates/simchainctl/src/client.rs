use serde::de::DeserializeOwned;
use simchain_common::control_api::{
    AbortJobResponse, ApiErrorEnvelope, ApplyReport, ComponentControlResponse, ConfigPatchRequest,
    ConfigResponse, DegradeJobRequest, JobCheckpointResponse, JobCreatedResponse, JobDetail,
    JobEventsResponse, JobListResponse, MineJobRequest, PartitionJobRequest,
    ReleaseCheckpointRequest, ReorgJobRequest, ScenarioJobRequest, SetComponentStateRequest,
    SpamBurstJobRequest, StatusResponse, API_PREFIX,
};
use simchain_common::internal_api::DesiredState;
use std::fmt;

#[derive(Debug)]
pub enum ClientError {
    Unavailable(String),
    Authentication(String),
    Api(String),
    Decode(String),
    Output(String),
    Local(String),
    Timeout(String),
    Interrupted(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message)
            | Self::Authentication(message)
            | Self::Api(message)
            | Self::Decode(message)
            | Self::Output(message)
            | Self::Local(message)
            | Self::Timeout(message)
            | Self::Interrupted(message) => formatter.write_str(message),
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

    pub fn patch_config(&self, request: &ConfigPatchRequest) -> Result<ApplyReport, ClientError> {
        self.patch_json(&format!("{API_PREFIX}/config"), request)
    }

    pub fn set_mining_state(
        &self,
        state: DesiredState,
    ) -> Result<ComponentControlResponse, ClientError> {
        self.set_component_state("mining", state)
    }

    pub fn set_spam_state(
        &self,
        state: DesiredState,
    ) -> Result<ComponentControlResponse, ClientError> {
        self.set_component_state("spam", state)
    }

    pub fn start_reorg(
        &self,
        request: &ReorgJobRequest,
        idempotency_key: Option<&str>,
    ) -> Result<JobCreatedResponse, ClientError> {
        self.post_json(
            &format!("{API_PREFIX}/jobs/reorg"),
            request,
            idempotency_key,
        )
    }

    pub fn start_scenario(
        &self,
        yaml: String,
        idempotency_key: Option<&str>,
    ) -> Result<JobCreatedResponse, ClientError> {
        self.post_json(
            &format!("{API_PREFIX}/jobs/scenario"),
            &ScenarioJobRequest { yaml },
            idempotency_key,
        )
    }

    pub fn start_mine(
        &self,
        request: &MineJobRequest,
        idempotency_key: Option<&str>,
    ) -> Result<JobCreatedResponse, ClientError> {
        self.post_json(&format!("{API_PREFIX}/jobs/mine"), request, idempotency_key)
    }

    pub fn start_spam_burst(
        &self,
        request: &SpamBurstJobRequest,
        idempotency_key: Option<&str>,
    ) -> Result<JobCreatedResponse, ClientError> {
        self.post_json(
            &format!("{API_PREFIX}/jobs/spam-burst"),
            request,
            idempotency_key,
        )
    }

    pub fn start_partition(
        &self,
        request: &PartitionJobRequest,
        idempotency_key: Option<&str>,
    ) -> Result<JobCreatedResponse, ClientError> {
        self.post_json(
            &format!("{API_PREFIX}/jobs/partition"),
            request,
            idempotency_key,
        )
    }

    pub fn start_degrade(
        &self,
        request: &DegradeJobRequest,
        idempotency_key: Option<&str>,
    ) -> Result<JobCreatedResponse, ClientError> {
        self.post_json(
            &format!("{API_PREFIX}/jobs/degrade"),
            request,
            idempotency_key,
        )
    }

    pub fn checkpoint(
        &self,
        job_id: &str,
        checkpoint: &str,
    ) -> Result<JobCheckpointResponse, ClientError> {
        self.get(&format!(
            "{API_PREFIX}/jobs/{job_id}/checkpoints/{checkpoint}"
        ))
    }

    pub fn release_checkpoint(
        &self,
        job_id: &str,
        checkpoint: &str,
        generation: u64,
    ) -> Result<JobCheckpointResponse, ClientError> {
        self.post_json(
            &format!("{API_PREFIX}/jobs/{job_id}/checkpoints/{checkpoint}/release"),
            &ReleaseCheckpointRequest { generation },
            None,
        )
    }

    pub fn jobs(&self) -> Result<JobListResponse, ClientError> {
        self.get(&format!("{API_PREFIX}/jobs"))
    }

    pub fn job(&self, job_id: &str) -> Result<JobDetail, ClientError> {
        self.get(&format!("{API_PREFIX}/jobs/{job_id}"))
    }

    pub fn job_events(
        &self,
        job_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<JobEventsResponse, ClientError> {
        self.get(&format!(
            "{API_PREFIX}/jobs/{job_id}/events?after={after}&limit={limit}"
        ))
    }

    pub fn abort_job(&self, job_id: &str) -> Result<AbortJobResponse, ClientError> {
        let path = format!("{API_PREFIX}/jobs/{job_id}/abort");
        let url = format!("{}{path}", self.base_url);
        let mut request = minreq::post(&url).with_timeout(10);
        if let Some(token) = self.token.as_deref() {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        let response = request
            .send()
            .map_err(|error| ClientError::Unavailable(format!("cannot reach {url}: {error}")))?;
        self.decode(&url, response)
    }

    fn set_component_state(
        &self,
        component: &str,
        state: DesiredState,
    ) -> Result<ComponentControlResponse, ClientError> {
        let path = format!("{API_PREFIX}/{component}/state");
        let url = format!("{}{path}", self.base_url);
        let body = serde_json::to_string(&SetComponentStateRequest { state })
            .map_err(|error| ClientError::Output(error.to_string()))?;
        let mut request = minreq::put(&url)
            .with_timeout(35)
            .with_header("Content-Type", "application/json")
            .with_body(body);
        if let Some(token) = self.token.as_deref() {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        let response = request
            .send()
            .map_err(|error| ClientError::Unavailable(format!("cannot reach {url}: {error}")))?;
        self.decode(&url, response)
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
        self.decode(&url, response)
    }

    fn post_json<B: serde::Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        idempotency_key: Option<&str>,
    ) -> Result<R, ClientError> {
        let url = format!("{}{path}", self.base_url);
        let body =
            serde_json::to_string(body).map_err(|error| ClientError::Output(error.to_string()))?;
        let mut request = minreq::post(&url)
            .with_timeout(35)
            .with_header("Content-Type", "application/json")
            .with_body(body);
        if let Some(token) = self.token.as_deref() {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        if let Some(key) = idempotency_key {
            request = request.with_header("Idempotency-Key", key);
        }
        let response = request
            .send()
            .map_err(|error| ClientError::Unavailable(format!("cannot reach {url}: {error}")))?;
        self.decode(&url, response)
    }

    fn patch_json<B: serde::Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, ClientError> {
        let url = format!("{}{path}", self.base_url);
        let body =
            serde_json::to_string(body).map_err(|error| ClientError::Output(error.to_string()))?;
        let mut request = minreq::patch(&url)
            .with_timeout(35)
            .with_header("Content-Type", "application/json")
            .with_body(body);
        if let Some(token) = self.token.as_deref() {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        let response = request
            .send()
            .map_err(|error| ClientError::Unavailable(format!("cannot reach {url}: {error}")))?;
        self.decode(&url, response)
    }

    fn decode<T: DeserializeOwned>(
        &self,
        url: &str,
        response: minreq::Response,
    ) -> Result<T, ClientError> {
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

    #[test]
    fn mining_pause_uses_authenticated_versioned_put() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("PUT /api/v1/mining/state HTTP/1.1"));
            assert!(request.contains("Authorization: Bearer secret"));
            assert!(request.contains(r#"{"state":"paused"}"#));

            let response_body = serde_json::to_string(&ComponentControlResponse {
                component: "mining".to_string(),
                desired_state: DesiredState::Paused,
                effective_state: DesiredState::Paused,
                phase: simchain_common::internal_api::WorkerPhase::Paused,
                effective_generation: 2,
            })
            .expect("response JSON");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("response");
        });

        let client = ControlClient::new(format!("http://{address}"), Some("secret".to_string()));
        let response = client
            .set_mining_state(DesiredState::Paused)
            .expect("pause response");
        assert_eq!(response.effective_state, DesiredState::Paused);
        server.join().expect("server");
    }

    #[test]
    fn config_patch_uses_authenticated_generation_cas() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("PATCH /api/v1/config HTTP/1.1"));
            assert!(request.contains("Authorization: Bearer secret"));
            assert!(request.contains(r#""base_generation":7"#));
            assert!(request.contains(r#""BLOCK_INTERVAL_MEAN_SECS":"12""#));

            let response_body = serde_json::to_string(&ApplyReport {
                changed: true,
                components_applied: vec!["mining".to_string(), "spam".to_string()],
                generation: 8,
                logs: Vec::new(),
                warnings: Vec::new(),
            })
            .expect("response JSON");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("response");
        });

        let client = ControlClient::new(format!("http://{address}"), Some("secret".to_string()));
        let response = client
            .patch_config(&ConfigPatchRequest {
                settings: [("BLOCK_INTERVAL_MEAN_SECS".to_string(), "12".to_string())]
                    .into_iter()
                    .collect(),
                base_generation: Some(7),
            })
            .expect("patch response");
        assert_eq!(response.generation, 8);
        server.join().expect("server");
    }

    #[test]
    fn spam_pause_uses_authenticated_versioned_put() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("PUT /api/v1/spam/state HTTP/1.1"));
            assert!(request.contains("Authorization: Bearer secret"));

            let response_body = serde_json::to_string(&ComponentControlResponse {
                component: "spam".to_string(),
                desired_state: DesiredState::Paused,
                effective_state: DesiredState::Paused,
                phase: simchain_common::internal_api::WorkerPhase::Paused,
                effective_generation: 3,
            })
            .expect("response JSON");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("response");
        });

        let client = ControlClient::new(format!("http://{address}"), Some("secret".to_string()));
        let response = client
            .set_spam_state(DesiredState::Paused)
            .expect("pause response");
        assert_eq!(response.component, "spam");
        server.join().expect("server");
    }

    #[test]
    fn reorg_start_uses_auth_and_idempotency_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("POST /api/v1/jobs/reorg HTTP/1.1"));
            assert!(request.contains("Authorization: Bearer secret"));
            assert!(request.contains("Idempotency-Key: retry-1"));
            assert!(request.contains(r#""depth":4"#));
            assert!(request.contains(r#""empty":true"#));

            let response_body = serde_json::to_string(&JobCreatedResponse {
                job_id: "job-4".to_string(),
                state: simchain_common::control_api::JobState::Starting,
                reused: false,
            })
            .expect("response JSON");
            let response = format!(
                "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("response");
        });

        let client = ControlClient::new(format!("http://{address}"), Some("secret".to_string()));
        let response = client
            .start_reorg(
                &ReorgJobRequest {
                    depth: 4,
                    empty: true,
                    ..ReorgJobRequest::default()
                },
                Some("retry-1"),
            )
            .expect("job response");
        assert_eq!(response.job_id, "job-4");
        server.join().expect("server");
    }

    #[test]
    fn scenario_start_uses_authenticated_json_envelope_and_retry_header() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 8192];
            let read = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("POST /api/v1/jobs/scenario HTTP/1.1"));
            assert!(request.contains("Authorization: Bearer secret"));
            assert!(request.contains("Idempotency-Key: scenario-retry"));
            assert!(request.contains(r#""yaml":"version: 1\nsteps: []\n""#));

            let response_body = serde_json::to_string(&JobCreatedResponse {
                job_id: "job-scenario".to_string(),
                state: simchain_common::control_api::JobState::Starting,
                reused: false,
            })
            .expect("response JSON");
            let response = format!(
                "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("response");
        });

        let client = ControlClient::new(format!("http://{address}"), Some("secret".to_string()));
        let response = client
            .start_scenario(
                "version: 1\nsteps: []\n".to_string(),
                Some("scenario-retry"),
            )
            .expect("scenario response");
        assert_eq!(response.job_id, "job-scenario");
        server.join().expect("server");
    }

    #[test]
    fn checkpoint_release_posts_the_pinned_generation() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(
                request.starts_with("POST /api/v1/jobs/job-1/checkpoints/held/release HTTP/1.1")
            );
            assert!(request.contains("Authorization: Bearer secret"));
            assert!(request.contains(r#"{"generation":42}"#));

            let response_body = serde_json::to_string(&JobCheckpointResponse {
                job_id: "job-1".to_string(),
                checkpoint: simchain_common::control_api::JobCheckpoint {
                    name: "held".to_string(),
                    generation: 42,
                    state: simchain_common::control_api::CheckpointState::Released,
                    pause: true,
                    timeout_secs: Some(60),
                    step_index: 1,
                    arrived_at_ms: Some(1),
                    released_at_ms: Some(2),
                    live_summary: None,
                },
            })
            .expect("response JSON");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("response");
        });

        let client = ControlClient::new(format!("http://{address}"), Some("secret".to_string()));
        let response = client
            .release_checkpoint("job-1", "held", 42)
            .expect("release response");
        assert_eq!(response.checkpoint.generation, 42);
        server.join().expect("server");
    }
}
