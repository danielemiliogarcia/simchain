use serde::{Deserialize, Serialize};

/// Stable machine-readable errors. Clients branch on these values, never on
/// human-facing messages.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ValidationFailed,
    StaleRevision,
    OperationInProgress,
    ApplyInProgress,
    ComponentUnavailable,
    FaucetDeliveryPending,
    InsufficientFaucetFunds,
    PreparedInputsConflicted,
    FaucetUnavailable,
    FaucetPriorityInvariantFailed,
    JobNotFound,
    CheckpointConflict,
    RollbackFailed,
    RpcUnavailable,
    Unauthorized,
    Internal,
}

impl ErrorCode {
    pub fn http_status(self) -> u16 {
        match self {
            Self::ValidationFailed => 422,
            Self::StaleRevision
            | Self::OperationInProgress
            | Self::ApplyInProgress
            | Self::CheckpointConflict
            | Self::FaucetDeliveryPending
            | Self::InsufficientFaucetFunds
            | Self::PreparedInputsConflicted => 409,
            Self::ComponentUnavailable
            | Self::RpcUnavailable
            | Self::FaucetUnavailable
            | Self::FaucetPriorityInvariantFailed => 503,
            Self::JobNotFound => 404,
            Self::Unauthorized => 401,
            Self::RollbackFailed | Self::Internal => 500,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ErrorDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub cause: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RollbackReport {
    pub desired_state_preserved: bool,
    pub runtime_restored: bool,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<ErrorDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback: Option<RollbackReport>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Vec::new(),
            rollback: None,
        }
    }

    pub fn with_details(mut self, details: Vec<ErrorDetail>) -> Self {
        self.details = details;
        self
    }

    pub fn envelope(&self) -> ApiErrorEnvelope {
        ApiErrorEnvelope {
            error: self.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ApiErrorEnvelope {
    pub error: ApiError,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_error_codes_round_trip_as_snake_case() {
        let error = ApiError::new(ErrorCode::OperationInProgress, "busy");
        let json = serde_json::to_string(&error.envelope()).expect("serialize");
        assert!(json.contains("operation_in_progress"));
        let decoded: ApiErrorEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.error.code, ErrorCode::OperationInProgress);
        assert_eq!(decoded.error.code.http_status(), 409);
    }
}
