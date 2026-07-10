//! Node-wallet spam engine: spam is sent with sendtoaddress/sendmany on the
//! miner node wallets, so bitcoind does coin selection, change handling and
//! signing. This is the original engine, kept selectable with
//! USE_RAW_TX_SPAM=false: its transactions are exactly what a real wallet
//! produces, but throughput is bound by the wallet lock and degrades as the
//! wallet's tx history grows (see SETTINGS.md "Full blocks").

use crate::common::rpc_retry;
use bitcoincore_rpc::{
    bitcoin::{Address, Amount, Network, Txid},
    Client, RpcApi,
};
use serde_json::json;
use std::{thread, time::Duration};

// A fan-out UTXO is ~0.1 BTC. Count only confirmed UTXOs in this band as
// "spammable": it excludes any 546-sat dust the wallet may receive (below the
// floor) and the large coinbase / change UTXOs (above the ceiling), so the
// count reflects the pool of independent branches actually available to spam
// from, not a wallet clogged with dust.
const SPAMMABLE_MIN_BTC: f64 = 0.001;
const SPAMMABLE_MAX_BTC: f64 = 0.5;

fn get_new_wallet_address(wallet: &Client) -> Address {
    let address = rpc_retry("get new wallet address", || {
        wallet.get_new_address(None, None)
    });
    address.require_network(Network::Regtest).unwrap()
}

fn spammable_utxos(wallet: &Client) -> u64 {
    let min = Amount::from_btc(SPAMMABLE_MIN_BTC).unwrap();
    let max = Amount::from_btc(SPAMMABLE_MAX_BTC).unwrap();
    rpc_retry("list spammable wallet UTXOs", || {
        wallet.list_unspent(Some(1), None, None, None, None)
    })
    .iter()
    .filter(|u| u.amount >= min && u.amount <= max)
    .count() as u64
}

// Keep the wallet supplied with independent fan-out UTXOs. The mempool limits a
// chain of unconfirmed transactions to 25, so a wallet spending from a single
// UTXO can never place more than 25 txs per block; `target` independent UTXOs
// let it build that many parallel chains. When the spammable pool drops below
// `need` -- at startup (only coinbases exist), or after a reorg un-confirms the
// wallet's recent change, or when incoming dust is all that is left -- split
// confirmed funds into `target` fresh UTXOs. A cheap no-op (one list_unspent)
// when the pool is healthy, so it is safe to call every block.
fn ensure_fanout(wallet: &Client, name: &str, need: u64, target: u64) {
    if spammable_utxos(wallet) >= need {
        return;
    }

    let trusted = rpc_retry("get wallet balance for fan-out", || wallet.get_balances())
        .mine
        .trusted
        .to_btc();
    // 0.1 BTC per branch funds years of dust spam; scale down if the wallet is
    // smaller than target * 0.1 (keep 20% margin for fees).
    let per_output = (trusted * 0.8 / target as f64).min(0.1);
    let per_output = (per_output * 1e8).floor() / 1e8;
    if per_output <= 0.0 {
        // Funds are tied up in unconfirmed spam; a block will free them.
        tracing::warn!("Wallet '{name}' has no confirmed funds to fan out yet, deferring");
        return;
    }

    tracing::info!("Wallet '{name}' low on spammable UTXOs, splitting funds into {target} UTXOs of {per_output} BTC each");
    let mut outputs = serde_json::Map::new();
    while outputs.len() < target as usize {
        let address = get_new_wallet_address(wallet);
        outputs.insert(address.to_string(), json!(per_output));
    }
    match wallet.call::<String>("sendmany", &[json!(""), json!(outputs)]) {
        Ok(txid) => tracing::info!("Fan-out tx {txid} sent, waiting for it to confirm..."),
        Err(e) => {
            tracing::warn!("Wallet '{name}' fan-out failed ({e}), retrying next block");
            return;
        }
    }

    loop {
        if spammable_utxos(wallet) >= need {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    tracing::info!("Wallet '{name}' fan-out confirmed");
}

// Send `count` txs and report how many actually made it, so empty blocks
// are noticed (a silent wallet error would defeat the spammer's purpose).
// Returns the accepted txids so a fraction of them can be fee-bumped.
fn send_spam_tx(from: &Client, to_address: &Address, count: u64, replaceable: bool) -> Vec<Txid> {
    // 546 sats is the dust limit for P2PKH outputs, the highest floor among
    // the common output types (bech32 is 294), so this amount is safely
    // above dust no matter what address type receives it.
    let amount = Amount::from_sat(546);
    let mut txids = Vec::new();
    let mut first_error: Option<String> = None;
    let replaceable = if replaceable { Some(true) } else { None };
    for _ in 0..count {
        match from.send_to_address(
            to_address,
            amount,
            None,
            None,
            None,
            replaceable,
            None,
            None,
        ) {
            Ok(txid) => txids.push(txid),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
            }
        }
    }
    if let Some(error) = first_error {
        tracing::warn!(
            "only {}/{count} spam txs accepted, first error: {error}",
            txids.len()
        );
    }
    txids
}

// Batch mode: send `count` txs of `to_addresses.len()` outputs each, one
// sendmany RPC per tx (546 sats per output, same dust-safe amount as the
// sequential mode). The same address set is reused for every batch -- sendmany
// only needs the keys of ONE tx to be distinct -- which is also what real
// exchange-payout traffic looks like. Reports partial acceptance like
// send_spam_tx and returns the txids so a fraction can be fee-bumped.
fn send_spam_batch(
    from: &Client,
    to_addresses: &[Address],
    count: u64,
    replaceable: bool,
) -> Vec<Txid> {
    let mut amounts = serde_json::Map::new();
    for address in to_addresses {
        amounts.insert(address.to_string(), json!(0.00000546));
    }
    // sendmany positional params: dummy, amounts, minconf, comment,
    // subtractfeefrom, replaceable
    let params = [
        json!(""),
        json!(amounts),
        json!(1),
        json!(""),
        json!([]),
        json!(replaceable),
    ];
    let mut txids = Vec::new();
    let mut first_error: Option<String> = None;
    for _ in 0..count {
        match from.call::<String>("sendmany", &params) {
            Ok(txid) => txids.push(txid.parse().expect("bitcoind returned an invalid txid")),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
            }
        }
    }
    if let Some(error) = first_error {
        tracing::warn!(
            "only {}/{count} sendmany batches accepted, first error: {error}",
            txids.len()
        );
    }
    txids
}

// Fee-bump (RBF) up to `count` of the just-sent spam txs, so the mempool
// carries real BIP125 replacements for downstream code to handle. Bump
// newest-first: the latest txs are the tips of the unconfirmed chains, and
// a tx with in-wallet descendants cannot be bumped.
fn bump_spam_txs(wallet: &Client, label: &str, txids: &[Txid], count: u64) {
    let mut bumped = 0;
    let mut first_error: Option<String> = None;
    for txid in txids.iter().rev() {
        if bumped >= count {
            break;
        }
        match wallet.call::<serde_json::Value>("bumpfee", &[json!(txid.to_string())]) {
            Ok(_) => bumped += 1,
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
            }
        }
    }
    match first_error {
        Some(error) if bumped < count => {
            tracing::info!(
                "{label} => Fee-bumped (RBF) {bumped}/{count} spam txs, first error: {error}"
            )
        }
        _ => tracing::info!("{label} => Fee-bumped (RBF) {bumped} spam txs"),
    }
}

// One wallet's full spam round: top up the fan-out pool if it ran low, send
// this wallet's share of the block's spam, then fee-bump its own txs when RBF
// traffic is enabled. Each wallet lives on its own node, so running one round
// per thread gives two independent RPC pipelines against two independent
// bitcoind processes and roughly halves the send cycle compared to spamming
// the wallets one after the other.
#[allow(clippy::too_many_arguments)]
pub fn spam_round(
    wallet: &Client,
    wallet_name: &str,
    label: &str,
    share: u64,
    fanout_need: u64,
    fanout_utxos: u64,
    seq_addr: &Address,
    batch_addrs: &[Address],
    replaceable: bool,
    replaces: u64,
) -> Vec<Txid> {
    if fanout_utxos > 0 {
        ensure_fanout(wallet, wallet_name, fanout_need, fanout_utxos);
    }
    let txids = if !batch_addrs.is_empty() {
        tracing::info!(
            "{label} => Spamming {share} sendmany batches of {} outputs to burn addresses",
            batch_addrs.len()
        );
        send_spam_batch(wallet, batch_addrs, share, replaceable)
    } else {
        tracing::info!("{label} => Spamming {share} transactions to address {seq_addr}");
        send_spam_tx(wallet, seq_addr, share, replaceable)
    };
    if replaceable {
        bump_spam_txs(wallet, label, &txids, replaces);
    }
    txids
}
