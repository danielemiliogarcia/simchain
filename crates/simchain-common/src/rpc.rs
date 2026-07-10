//! Bitcoin Core RPC client construction and the shared retry policy.

use crate::error::CommonError;
use bitcoincore_rpc::{jsonrpc, Client, RpcApi};
use std::thread;
use std::time::Duration;

/// A node busy with a big mempool or mid-block-assembly can take longer than
/// the default 15s RPC timeout (a large `sendmany` alone can, and so can
/// disconnecting blocks with hundreds of txs during a reorg), and the client
/// then dies on a `WouldBlock` socket error. Build every client with a
/// generous timeout instead; healthy calls are unaffected.
pub const RPC_TIMEOUT_SECS: u64 = 300;

/// 8 attempts with the backoff in [`rpc_retry`] give ~61s of tolerance for
/// fast-failing errors (connection refused while a node reboots), so a normal
/// bitcoind restart is ridden out in-process instead of crashing into a
/// container restart. Timeouts are far slower per attempt and stay bounded
/// regardless.
pub const RPC_RETRY_ATTEMPTS: u32 = 8;

/// Retry a replay-safe RPC call with exponential backoff. Panics after the
/// bounded attempt count so compose `restart: on-failure` remains the
/// backstop for a wedged node. Most uses must be read-only; `getnewaddress` is
/// also safe because replay only advances the wallet's address index. Do not
/// use this for non-idempotent actions such as mining or sending funds.
pub fn rpc_retry<T>(what: &str, mut call: impl FnMut() -> Result<T, bitcoincore_rpc::Error>) -> T {
    let mut delay = Duration::from_millis(500);
    for attempt in 1..=RPC_RETRY_ATTEMPTS {
        match call() {
            Ok(value) => return value,
            Err(error) if attempt == RPC_RETRY_ATTEMPTS => {
                tracing::error!("RPC {what} failed after {RPC_RETRY_ATTEMPTS} attempts: {error}");
                panic!("RPC {what} failed after {RPC_RETRY_ATTEMPTS} attempts: {error}")
            }
            Err(error) => {
                tracing::warn!(
                    "RPC {what} failed ({error}), retry {attempt}/{RPC_RETRY_ATTEMPTS} in {delay:?}"
                );
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
    unreachable!()
}

/// Poll until a node responds to `getblockcount`. Callers choose the polling
/// interval to match their startup behavior while sharing the same readiness
/// check and log message across tools.
pub fn wait_for_rpc(client: &Client, name: &str, poll_interval: Duration) {
    loop {
        match client.get_block_count() {
            Ok(_) => return,
            Err(_) => {
                tracing::info!("Waiting for {name} RPC...");
                thread::sleep(poll_interval);
            }
        }
    }
}

/// Poll until a node reports at least `height`. This is useful after mining a
/// block on one node before continuing work on another node that must share
/// the same chain tip.
pub fn wait_for_height(client: &Client, height: u64, poll_interval: Duration) {
    loop {
        match client.get_block_count() {
            Ok(current) if current >= height => return,
            _ => thread::sleep(poll_interval),
        }
    }
}

/// Build a Bitcoin Core RPC [`Client`] with the shared [`RPC_TIMEOUT_SECS`]
/// timeout. Fails with [`CommonError::InvalidRpcUrl`] if `rpc_url` does not
/// parse.
pub fn create_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> Result<Client, CommonError> {
    Ok(Client::from_jsonrpc(create_jsonrpc_client(
        rpc_url, rpc_user, rpc_pass,
    )?))
}

/// Build a wallet-scoped RPC client. Wallet paths stay stable even when a node
/// has multiple wallets loaded, unlike the generic node RPC endpoint.
pub fn create_wallet_client(
    node_rpc_url: &str,
    wallet_name: &str,
    rpc_user: &str,
    rpc_pass: &str,
) -> Result<Client, CommonError> {
    create_client(
        &format!("{node_rpc_url}/wallet/{wallet_name}"),
        rpc_user,
        rpc_pass,
    )
}

/// Build the underlying [`jsonrpc::Client`]. Exposed for callers that need the
/// raw JSON-RPC client (the spammer's raw-transaction engine) rather than the
/// wrapped [`Client`]. Fails with [`CommonError::InvalidRpcUrl`] if `rpc_url`
/// does not parse.
pub fn create_jsonrpc_client(
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
) -> Result<jsonrpc::Client, CommonError> {
    let (user, pass) = (rpc_user.to_string(), Some(rpc_pass.to_string()));
    let transport = jsonrpc::simple_http::SimpleHttpTransport::builder()
        .url(rpc_url)
        .map_err(|source| CommonError::InvalidRpcUrl {
            url: rpc_url.to_string(),
            source,
        })?
        .auth(user, pass)
        .timeout(Duration::from_secs(RPC_TIMEOUT_SECS))
        .build();
    Ok(jsonrpc::Client::with_transport(transport))
}
