//! Synchronous clients for authenticated worker APIs. Control-plane handlers
//! run these calls on blocking workers; status sampling is already blocking.

use crate::backend::{MiningControlBackend, SpamControlBackend};
use serde::de::DeserializeOwned;
use simchain_common::control_api::ApiErrorEnvelope;
use simchain_common::internal_api::{
    CommandAck, DesiredState, LeaseReleaseRequest, LeaseRenewRequest, LeaseRequest,
    MiningWorkerStatus, SetMiningPolicyRequest, SetSpamPolicyRequest, SetStateRequest,
    SpamWorkerStatus, INTERNAL_API_PREFIX,
};
use simchain_common::live_tuning::{MiningTuning, SpamTuning};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct MiningClient {
    base_url: String,
    token: String,
    request_sequence: AtomicU64,
}

impl MiningClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            request_sequence: AtomicU64::new(1),
        }
    }

    fn request_id(&self, operation: &str) -> String {
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed);
        format!("control-{operation}-{}-{sequence}", now_ms())
    }

    fn request<T: serde::Serialize, R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&T>,
    ) -> anyhow::Result<R> {
        let url = format!("{}{}", self.base_url, path);
        let mut request = match method {
            Method::Get => minreq::get(&url),
            Method::Put => minreq::put(&url),
            Method::Post => minreq::post(&url),
            Method::Delete => minreq::delete(&url),
        }
        .with_timeout(35)
        .with_header("Authorization", format!("Bearer {}", self.token));
        if let Some(body) = body {
            request = request
                .with_header("Content-Type", "application/json")
                .with_body(serde_json::to_string(body)?);
        }
        let response = request.send()?;
        let text = response.as_str()?;
        if !(200..300).contains(&response.status_code) {
            let message = serde_json::from_str::<ApiErrorEnvelope>(text)
                .map(|envelope| envelope.error.message)
                .unwrap_or_else(|_| format!("HTTP {}: {text}", response.status_code));
            anyhow::bail!("mining worker rejected request: {message}");
        }
        Ok(serde_json::from_str(text)?)
    }
}

pub struct SpamClient {
    base_url: String,
    token: String,
    request_sequence: AtomicU64,
}

impl SpamClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            request_sequence: AtomicU64::new(1),
        }
    }

    fn request_id(&self, operation: &str) -> String {
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed);
        format!("control-{operation}-{}-{sequence}", now_ms())
    }

    fn request<T: serde::Serialize, R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&T>,
    ) -> anyhow::Result<R> {
        let url = format!("{}{}", self.base_url, path);
        let mut request = match method {
            Method::Get => minreq::get(&url),
            Method::Put => minreq::put(&url),
            Method::Post => minreq::post(&url),
            Method::Delete => minreq::delete(&url),
        }
        .with_timeout(35)
        .with_header("Authorization", format!("Bearer {}", self.token));
        if let Some(body) = body {
            request = request
                .with_header("Content-Type", "application/json")
                .with_body(serde_json::to_string(body)?);
        }
        let response = request.send()?;
        let text = response.as_str()?;
        if !(200..300).contains(&response.status_code) {
            let message = serde_json::from_str::<ApiErrorEnvelope>(text)
                .map(|envelope| envelope.error.message)
                .unwrap_or_else(|_| format!("HTTP {}: {text}", response.status_code));
            anyhow::bail!("spam worker rejected request: {message}");
        }
        Ok(serde_json::from_str(text)?)
    }

    fn send_policy(
        &self,
        generation: u64,
        policy: SpamTuning,
        rollback: bool,
    ) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Put,
            &format!("{INTERNAL_API_PREFIX}/config"),
            Some(&SetSpamPolicyRequest {
                generation,
                policy,
                request_id: self.request_id("spam-config"),
                rollback,
            }),
        )
    }
}

impl SpamControlBackend for SpamClient {
    fn status(&self) -> anyhow::Result<SpamWorkerStatus> {
        self.request::<(), _>(Method::Get, &format!("{INTERNAL_API_PREFIX}/status"), None)
    }

    fn set_state(&self, state: DesiredState) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Put,
            &format!("{INTERNAL_API_PREFIX}/state"),
            Some(&SetStateRequest {
                state,
                request_id: self.request_id("spam-state"),
            }),
        )
    }

    fn set_policy(&self, generation: u64, policy: SpamTuning) -> anyhow::Result<CommandAck> {
        self.send_policy(generation, policy, false)
    }

    fn restore_policy(&self, generation: u64, policy: SpamTuning) -> anyhow::Result<CommandAck> {
        self.send_policy(generation, policy, true)
    }

    fn acquire_lease(&self, request: LeaseRequest) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Post,
            &format!("{INTERNAL_API_PREFIX}/leases"),
            Some(&request),
        )
    }

    fn renew_lease(
        &self,
        lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Post,
            &format!("{INTERNAL_API_PREFIX}/leases/{lease_id}/renew"),
            Some(&request),
        )
    }

    fn release_lease(
        &self,
        lease_id: &str,
        request: LeaseReleaseRequest,
    ) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Delete,
            &format!("{INTERNAL_API_PREFIX}/leases/{lease_id}"),
            Some(&request),
        )
    }
}

impl MiningControlBackend for MiningClient {
    fn status(&self) -> anyhow::Result<MiningWorkerStatus> {
        self.request::<(), _>(Method::Get, &format!("{INTERNAL_API_PREFIX}/status"), None)
    }

    fn set_state(&self, state: DesiredState) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Put,
            &format!("{INTERNAL_API_PREFIX}/state"),
            Some(&SetStateRequest {
                state,
                request_id: self.request_id("mining-state"),
            }),
        )
    }

    fn set_policy(&self, generation: u64, policy: MiningTuning) -> anyhow::Result<CommandAck> {
        self.send_policy(generation, policy, false)
    }

    fn restore_policy(&self, generation: u64, policy: MiningTuning) -> anyhow::Result<CommandAck> {
        self.send_policy(generation, policy, true)
    }

    #[allow(dead_code)] // Called by the Phase 4 job coordinator.
    fn acquire_lease(&self, request: LeaseRequest) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Post,
            &format!("{INTERNAL_API_PREFIX}/leases"),
            Some(&request),
        )
    }

    #[allow(dead_code)] // Called by the Phase 4 job coordinator.
    fn renew_lease(
        &self,
        lease_id: &str,
        request: LeaseRenewRequest,
    ) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Post,
            &format!("{INTERNAL_API_PREFIX}/leases/{lease_id}/renew"),
            Some(&request),
        )
    }

    #[allow(dead_code)] // Called by the Phase 4 job coordinator.
    fn release_lease(
        &self,
        lease_id: &str,
        request: LeaseReleaseRequest,
    ) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Delete,
            &format!("{INTERNAL_API_PREFIX}/leases/{lease_id}"),
            Some(&request),
        )
    }
}

impl MiningClient {
    fn send_policy(
        &self,
        generation: u64,
        policy: MiningTuning,
        rollback: bool,
    ) -> anyhow::Result<CommandAck> {
        self.request(
            Method::Put,
            &format!("{INTERNAL_API_PREFIX}/config"),
            Some(&SetMiningPolicyRequest {
                generation,
                policy,
                request_id: self.request_id("mining-config"),
                rollback,
            }),
        )
    }
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Put,
    Post,
    Delete,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
