//! Helpers shared by both spam engines (node_wallet_spammer and
//! raw_transaction_spammer): RPC client construction, burn addresses and
//! wallet readiness.

use bitcoincore_rpc::{
    bitcoin::{
        hashes::{hash160, Hash},
        Address, Amount, Network, ScriptBuf, WPubkeyHash,
    },
    Client, RpcApi,
};
use std::{thread, time::Duration};

// RPC client construction and env lookup are shared with the other tools; the
// raw-transaction engine needs the raw jsonrpc::Client, so re-export both here.
pub use simchain_common::{create_client, create_jsonrpc_client, env_or};

/// Retry a replay-safe RPC call with exponential backoff. Panics after the
/// bounded attempt count so compose `restart: on-failure` remains the
/// backstop for a wedged node. Most uses must be read-only; `getnewaddress` is
/// also safe because replay only advances the wallet's address index. Do not
/// use this for non-idempotent actions such as sending funds.
pub fn rpc_retry<T>(what: &str, mut call: impl FnMut() -> Result<T, bitcoincore_rpc::Error>) -> T {
    // 8 attempts with the backoff below give ~61s of tolerance for
    // fast-failing errors (connection refused while a node reboots), so a
    // normal bitcoind restart is ridden out in-process instead of crashing
    // into a container restart.
    const ATTEMPTS: u32 = 8;
    let mut delay = Duration::from_millis(500);
    for attempt in 1..=ATTEMPTS {
        match call() {
            Ok(value) => return value,
            Err(error) if attempt == ATTEMPTS => {
                tracing::error!("RPC {what} failed after {ATTEMPTS} attempts: {error}");
                panic!("RPC {what} failed after {ATTEMPTS} attempts: {error}")
            }
            Err(error) => {
                tracing::warn!(
                    "RPC {what} failed ({error}), retry {attempt}/{ATTEMPTS} in {delay:?}"
                );
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
    unreachable!()
}

// Wallets the spam is split across (node2 and node3). If a miner is ever
// added or removed, updating this constant keeps SPAM_TXS_PER_BLOCK meaning
// "total txs per block" for the user.
pub const MINER_COUNT: u64 = 2;

// Spam destinations are burn addresses (P2WPKH over the hash of a fixed tag,
// no known key), not wallet addresses. Dust paid to a wallet address lands in
// that wallet, and bitcoind's coin selection scans every UTXO on each send:
// the old cross-wallet spam grew each miner wallet by one UTXO per spam
// output (~18k per full block in batch mode) until the send cycle no longer
// fit inside the block interval. Burning the dust keeps the wallets lean --
// they only accumulate their own change -- at the cost of slowly draining
// them (~0.16 BTC per full block against a ~2550 BTC bootstrap balance).
pub fn burn_address(index: u64) -> Address {
    let hash = hash160::Hash::hash(format!("simchain-spam-burn-{index}").as_bytes());
    let script = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_raw_hash(hash));
    Address::from_script(&script, Network::Regtest).unwrap()
}

// Wait until the wallet exists and has at least 1 BTC of trusted (confirmed,
// mature) balance. Right after bootstrap each miner wallet has one mature
// 50 BTC coinbase, so this returns quickly; it only really waits when the
// spammer starts before the mining controller finishes funding.
pub fn wait_for_funds(wallet: &Client, name: &str) {
    tracing::info!("Waiting for wallet '{name}' funds to mature...");
    let minimum = Amount::from_btc(1.0).unwrap();
    let mut iterations = 0u64;
    loop {
        match wallet.get_balances() {
            Ok(balances) if balances.mine.trusted >= minimum => return,
            Ok(balances) => {
                if iterations > 0 && iterations.is_multiple_of(60) {
                    tracing::info!(
                        "Still waiting for wallet '{name}': trusted balance {:.8} BTC < 1 BTC (coinbase maturity)",
                        balances.mine.trusted.to_btc()
                    );
                }
            }
            Err(error) => {
                if iterations > 0 && iterations.is_multiple_of(60) {
                    tracing::info!(
                        "Still waiting for wallet '{name}': not loaded yet (the mining controller creates it during bootstrap) — {error}"
                    );
                }
            }
        }
        iterations += 1;
        thread::sleep(Duration::from_millis(500));
    }
}
