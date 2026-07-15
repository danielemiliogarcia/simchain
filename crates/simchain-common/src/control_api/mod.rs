//! Versioned public control-plane DTOs shared by the server and first-party
//! clients. Domain code owns behavior; these types only define transport
//! shapes and must stay free of Docker- or UI-specific dependencies.

mod config;
mod error;
mod status;

pub use config::{
    ApplyMode, ConfigResponse, EffectiveComponentConfig, SchemaResponse, SettingSchema,
};
pub use error::{ApiError, ApiErrorEnvelope, ErrorCode, ErrorDetail, RollbackReport};
pub use status::{
    BlockSummary, Cadence, ComponentState, ExplorerStatus, FeeBucket, HealthResponse,
    ImpairmentSummary, MempoolSummary, OperationSummary, StatusResponse,
};

pub const API_PREFIX: &str = "/api/v1";
pub const DEFAULT_CONTROL_URL: &str = "http://127.0.0.1:8090";
