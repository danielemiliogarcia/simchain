//! Error types for the shared helpers.

use bitcoincore_rpc::jsonrpc;
use thiserror::Error;

/// Errors produced while constructing an RPC client from configuration.
#[derive(Debug, Error)]
pub enum CommonError {
    #[error("invalid RPC url '{url}': {source}")]
    InvalidRpcUrl {
        url: String,
        #[source]
        source: jsonrpc::simple_http::Error,
    },
}
