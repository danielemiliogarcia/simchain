//! Helpers shared by the simchain tools (mining-controller, reorg, spammer):
//! environment lookup and Bitcoin Core RPC client construction.
//!
//! These were previously copy-pasted into each tool. They live in one
//! workspace crate now so the binaries share a single implementation instead
//! of drifting apart.

use bitcoincore_rpc::{jsonrpc, Client};
use std::env;
use std::time::Duration;

/// A node busy with a big mempool or mid-block-assembly can take longer than
/// the default 15s RPC timeout (a large `sendmany` alone can, and so can
/// disconnecting blocks with hundreds of txs during a reorg), and the client
/// then dies on a `WouldBlock` socket error. Build every client with a
/// generous timeout instead; healthy calls are unaffected.
pub const RPC_TIMEOUT_SECS: u64 = 300;

/// Read `key` from the environment, falling back to `default` when it is unset.
pub fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Build a Bitcoin Core RPC [`Client`] with the shared [`RPC_TIMEOUT_SECS`]
/// timeout.
pub fn create_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> Client {
    Client::from_jsonrpc(create_jsonrpc_client(rpc_url, rpc_user, rpc_pass))
}

/// Build the underlying [`jsonrpc::Client`]. Exposed for callers that need the
/// raw JSON-RPC client (the spammer's raw-transaction engine) rather than the
/// wrapped [`Client`].
pub fn create_jsonrpc_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> jsonrpc::Client {
    let (user, pass) = (rpc_user.to_string(), Some(rpc_pass.to_string()));
    let transport = jsonrpc::simple_http::SimpleHttpTransport::builder()
        .url(rpc_url)
        .expect("invalid RPC url")
        .auth(user, pass)
        .timeout(Duration::from_secs(RPC_TIMEOUT_SECS))
        .build();
    jsonrpc::Client::with_transport(transport)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_or_returns_default_when_unset() {
        assert_eq!(
            env_or("SIMCHAIN_COMMON_DEFINITELY_UNSET_KEY", "fallback"),
            "fallback"
        );
    }

    #[test]
    fn env_or_returns_value_when_set() {
        // Unique key so this cannot collide with the unset-key test even if the
        // suite is ever run multi-threaded.
        env::set_var("SIMCHAIN_COMMON_SET_KEY", "actual");
        assert_eq!(env_or("SIMCHAIN_COMMON_SET_KEY", "fallback"), "actual");
        env::remove_var("SIMCHAIN_COMMON_SET_KEY");
    }
}
