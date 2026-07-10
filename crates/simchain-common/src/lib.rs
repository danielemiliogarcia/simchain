//! Helpers shared by the simchain tools (mining-controller, reorg, spammer):
//! environment lookup, Bitcoin Core RPC client construction and the shared
//! retry policy.
//!
//! These were previously copy-pasted into each tool. They live in one
//! workspace crate now so the binaries share a single implementation instead
//! of drifting apart.

mod address;
pub mod config;
mod error;
mod logging;
mod rpc;

pub use address::require_regtest_address;
pub use config::{CommonConfig, ConfigError, RpcUrl};
pub use error::CommonError;
pub use logging::init_tracing;
pub use rpc::{
    create_client, create_jsonrpc_client, create_wallet_client, rpc_retry, wait_for_height,
    wait_for_rpc, RPC_RETRY_ATTEMPTS, RPC_TIMEOUT_SECS,
};
