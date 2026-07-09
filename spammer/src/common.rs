//! Helpers shared by both spam engines (node_wallet_spammer and
//! raw_transaction_spammer): RPC client construction, burn addresses and
//! wallet readiness.

use bitcoincore_rpc::{
    bitcoin::{
        hashes::{hash160, Hash},
        Address, Amount, Network, ScriptBuf, WPubkeyHash,
    },
    jsonrpc, Client, RpcApi,
};
use std::{env, thread, time::Duration};

// A node busy with a big mempool or mid-block-assembly can take longer than
// the default 15s RPC timeout (a large sendmany alone can), and the client
// then dies on a WouldBlock socket error. Generous timeout instead; healthy
// calls are unaffected.
const RPC_TIMEOUT_SECS: u64 = 300;

// Wallets the spam is split across (node2 and node3). If a miner is ever
// added or removed, updating this constant keeps SPAM_TXS_PER_BLOCK meaning
// "total txs per block" for the user.
pub const MINER_COUNT: u64 = 2;

pub fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

pub fn create_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> Client {
    let (user, pass) = (rpc_user.to_string(), Some(rpc_pass.to_string()));
    let transport = jsonrpc::simple_http::SimpleHttpTransport::builder()
        .url(rpc_url)
        .expect("invalid RPC url")
        .auth(user, pass)
        .timeout(Duration::from_secs(RPC_TIMEOUT_SECS))
        .build();
    Client::from_jsonrpc(jsonrpc::client::Client::with_transport(transport))
}

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
    println!("Waiting for wallet '{name}' funds to mature...");
    loop {
        match wallet.get_balances() {
            Ok(balances) if balances.mine.trusted >= Amount::from_btc(1.0).unwrap() => return,
            _ => thread::sleep(Duration::from_millis(500)),
        }
    }
}
