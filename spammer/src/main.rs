use bitcoincore_rpc::{bitcoin::{hashes::{hash160, Hash}, Address, Amount, Network, ScriptBuf, Txid, WPubkeyHash}, Auth, Client, RpcApi};
use serde_json::json;
use std::{env, thread, time::Duration};

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn create_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> Client {
    Client::new(rpc_url, Auth::UserPass(rpc_user.to_string(), rpc_pass.to_string())).unwrap()
}

fn get_new_wallet_address(wallet: &Client) -> Address {
    let address = wallet.get_new_address(None, None).unwrap();
    address.require_network(Network::Regtest).unwrap()
}

// Spam destinations are burn addresses (P2WPKH over the hash of a fixed tag,
// no known key), not wallet addresses. Dust paid to a wallet address lands in
// that wallet, and bitcoind's coin selection scans every UTXO on each send:
// the old cross-wallet spam grew each miner wallet by one UTXO per spam
// output (~18k per full block in batch mode) until the send cycle no longer
// fit inside the block interval. Burning the dust keeps the wallets lean --
// they only accumulate their own change -- at the cost of slowly draining
// them (~0.16 BTC per full block against a ~2550 BTC bootstrap balance).
fn burn_address(index: u64) -> Address {
    let hash = hash160::Hash::hash(format!("simchain-spam-burn-{index}").as_bytes());
    let script = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_raw_hash(hash));
    Address::from_script(&script, Network::Regtest).unwrap()
}

// Wait until the wallet exists and has at least 1 BTC of trusted (confirmed,
// mature) balance. Right after bootstrap each miner wallet has one mature
// 50 BTC coinbase, so this returns quickly; it only really waits when the
// spammer starts before the mining controller finishes funding.
fn wait_for_funds(wallet: &Client, name: &str) {
    println!("Waiting for wallet '{name}' funds to mature...");
    loop {
        match wallet.get_balances() {
            Ok(balances) if balances.mine.trusted >= Amount::from_btc(1.0).unwrap() => return,
            _ => thread::sleep(Duration::from_millis(500)),
        }
    }
}

// A fan-out UTXO is ~0.1 BTC. Count only confirmed UTXOs in this band as
// "spammable": it excludes any 546-sat dust the wallet may receive (below the
// floor) and the large coinbase / change UTXOs (above the ceiling), so the
// count reflects the pool of independent branches actually available to spam
// from, not a wallet clogged with dust.
const SPAMMABLE_MIN_BTC: f64 = 0.001;
const SPAMMABLE_MAX_BTC: f64 = 0.5;

// Wallets the spam is split across (node2 and node3). If a miner is ever
// added or removed, updating this constant keeps SPAM_TXS_PER_BLOCK meaning
// "total txs per block" for the user.
const MINER_COUNT: u64 = 2;

fn spammable_utxos(wallet: &Client) -> u64 {
    let min = Amount::from_btc(SPAMMABLE_MIN_BTC).unwrap();
    let max = Amount::from_btc(SPAMMABLE_MAX_BTC).unwrap();
    wallet
        .list_unspent(Some(1), None, None, None, None)
        .unwrap()
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

    let trusted = wallet.get_balances().unwrap().mine.trusted.to_btc();
    // 0.1 BTC per branch funds years of dust spam; scale down if the wallet is
    // smaller than target * 0.1 (keep 20% margin for fees).
    let per_output = (trusted * 0.8 / target as f64).min(0.1);
    let per_output = (per_output * 1e8).floor() / 1e8;
    if per_output <= 0.0 {
        // Funds are tied up in unconfirmed spam; a block will free them.
        println!("Wallet '{name}' has no confirmed funds to fan out yet, deferring");
        return;
    }

    println!("Wallet '{name}' low on spammable UTXOs, splitting funds into {target} UTXOs of {per_output} BTC each");
    let mut outputs = serde_json::Map::new();
    while outputs.len() < target as usize {
        let address = get_new_wallet_address(wallet);
        outputs.insert(address.to_string(), json!(per_output));
    }
    match wallet.call::<String>("sendmany", &[json!(""), json!(outputs)]) {
        Ok(txid) => println!("Fan-out tx {txid} sent, waiting for it to confirm..."),
        Err(e) => {
            println!("Wallet '{name}' fan-out failed ({e}), retrying next block");
            return;
        }
    }

    loop {
        if spammable_utxos(wallet) >= need {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    println!("Wallet '{name}' fan-out confirmed");
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
        match from.send_to_address(&to_address, amount, None, None, None, replaceable, None, None) {
            Ok(txid) => txids.push(txid),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
            }
        }
    }
    if let Some(error) = first_error {
        println!("WARNING: only {}/{count} spam txs accepted, first error: {error}", txids.len());
    }
    txids
}

// Batch mode: send `count` txs of `to_addresses.len()` outputs each, one
// sendmany RPC per tx (546 sats per output, same dust-safe amount as the
// sequential mode). The same address set is reused for every batch -- sendmany
// only needs the keys of ONE tx to be distinct -- which is also what real
// exchange-payout traffic looks like. Reports partial acceptance like
// send_spam_tx and returns the txids so a fraction can be fee-bumped.
fn send_spam_batch(from: &Client, to_addresses: &[Address], count: u64, replaceable: bool) -> Vec<Txid> {
    let mut amounts = serde_json::Map::new();
    for address in to_addresses {
        amounts.insert(address.to_string(), json!(0.00000546));
    }
    // sendmany positional params: dummy, amounts, minconf, comment,
    // subtractfeefrom, replaceable
    let params = [json!(""), json!(amounts), json!(1), json!(""), json!([]), json!(replaceable)];
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
        println!("WARNING: only {}/{count} sendmany batches accepted, first error: {error}", txids.len());
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
            println!("{label} => Fee-bumped (RBF) {bumped}/{count} spam txs, first error: {error}")
        }
        _ => println!("{label} => Fee-bumped (RBF) {bumped} spam txs"),
    }
}

fn main() {
    // Every setting has a default matching docker-compose.yml, so the tool
    // also runs standalone with no environment at all.
    let enable_spam = env_or("ENABLE_SPAM", "true") == "true";
    if !enable_spam {
        println!("ENABLE_SPAM is not 'true', nothing to do, exiting");
        return;
    }

    // Total spam txs offered per block: the number a block explorer shows per
    // block (plus coinbase) as long as blocks are not already full. Splitting
    // it across the miner wallets is this tool's job, not the user's; the
    // legacy per-miner variable is still honored so old .env files keep working.
    let spam_txs_per_block: u64 = match env::var("SPAM_TXS_PER_BLOCK") {
        Ok(v) => v.parse().expect("SPAM_TXS_PER_BLOCK must be a positive integer"),
        Err(_) => match env::var("SPAM_PER_MINER_PER_BLOCK") {
            Ok(v) => {
                let per_miner: u64 = v.parse().expect("SPAM_PER_MINER_PER_BLOCK must be a positive integer");
                println!("WARNING: SPAM_PER_MINER_PER_BLOCK is deprecated, set SPAM_TXS_PER_BLOCK (total per block) instead; using {}", per_miner * MINER_COUNT);
                per_miner * MINER_COUNT
            }
            Err(_) => 100,
        },
    };
    // node2 takes the odd remainder so the two shares always sum to the total
    let spam2 = spam_txs_per_block.div_ceil(MINER_COUNT);
    let spam3 = spam_txs_per_block / MINER_COUNT;
    let fanout_utxos: u64 = env_or("SPAM_FANOUT_UTXOS", "50").parse().expect("SPAM_FANOUT_UTXOS must be a positive integer");
    // 0 = sequential mode: one sendtoaddress RPC per tx, so txs reach the
    // mempool one by one like p2p traffic on a real network. N > 0 = batch
    // mode: each spam tx is a single sendmany with N outputs, so one RPC call
    // places N payments -- the only way to FILL blocks on short intervals,
    // since sequential sending is bound by RPC round-trips (see SETTINGS.md
    // "Full blocks" for ready-made values).
    let sendmany_outputs: u64 = env_or("SPAM_SENDMANY_OUTPUTS", "0").parse().expect("SPAM_SENDMANY_OUTPUTS must be a non-negative integer");
    // RBF traffic: when enabled ("true" or "1") every spam tx signals BIP125
    // and the newest few of each batch get fee-bumped right after sending.
    let enable_replaces = matches!(env_or("ENABLE_SPAM_REPLACES", "false").as_str(), "true" | "1");
    let replaces_per_miner: u64 = env_or("SPAM_REPLACES_PER_MINER_PER_BLOCK", "5").parse().expect("SPAM_REPLACES_PER_MINER_PER_BLOCK must be a non-negative integer");
    let rpc_user = env_or("BTC_RPC_USER", "foo");
    let rpc_pass = env_or("BTC_RPC_PASS", "rpcpassword");
    let wallet2_name = env_or("NODE2_WALLET_NAME", "node2");
    let wallet3_name = env_or("NODE3_WALLET_NAME", "node3");

    let node1_url = env_or("NODE1_RPC_URL", "http://btc-simnet-node1:18443");
    let node2_url = env_or("NODE2_RPC_URL", "http://btc-simnet-node2:18443");
    let node3_url = env_or("NODE3_RPC_URL", "http://btc-simnet-node3:18443");

    let node1 = create_client(&node1_url, &rpc_user, &rpc_pass);
    // Wallet-scoped clients: they keep working even if the user loads extra
    // wallets on the nodes (the generic RPC path breaks with more than one)
    let wallet2 = create_client(&format!("{node2_url}/wallet/{wallet2_name}"), &rpc_user, &rpc_pass);
    let wallet3 = create_client(&format!("{node3_url}/wallet/{wallet3_name}"), &rpc_user, &rpc_pass);

    wait_for_funds(&wallet2, &wallet2_name);
    wait_for_funds(&wallet3, &wallet3_name);

    // Sequential mode target: one shared burn address -- reusing a single
    // address is exactly what real dust spam looks like.
    let seq_addr = burn_address(0);

    // Batch mode address pool: one fixed set of burn addresses, generated once
    // and shared by both miners' sendmany calls (the keys only need to be
    // distinct within one tx). Empty (and unused) in sequential mode.
    let batch_addrs: Vec<Address> = (1..=sendmany_outputs).map(burn_address).collect();

    // Cover a block's spam, but never require more branches than we fan out to.
    let fanout_need = spam2.min(fanout_utxos);

    // In a loop, if a new block is detected, spam transactions
    let mut spammed_at_block_height = 0;
    loop {
        let current_block_height = node1.get_block_count().unwrap();
        if current_block_height > spammed_at_block_height {
            spammed_at_block_height = current_block_height;
            // Top up the independent-UTXO pool if it ran low (fans out on the
            // first block, then only after a reorg or dust build-up depletes it).
            if fanout_utxos > 0 {
                ensure_fanout(&wallet2, &wallet2_name, fanout_need, fanout_utxos);
                ensure_fanout(&wallet3, &wallet3_name, fanout_need, fanout_utxos);
            }
            // spam transactions to burn addresses
            let (txids2, txids3) = if sendmany_outputs > 0 {
                println!("Node 2 => Spamming {spam2} sendmany batches of {sendmany_outputs} outputs to burn addresses");
                let txids2 = send_spam_batch(&wallet2, &batch_addrs, spam2, enable_replaces);
                println!("Node 3 => Spamming {spam3} sendmany batches of {sendmany_outputs} outputs to burn addresses");
                let txids3 = send_spam_batch(&wallet3, &batch_addrs, spam3, enable_replaces);
                (txids2, txids3)
            } else {
                println!("Node 2 => Spamming {spam2} transactions to address {seq_addr}");
                let txids2 = send_spam_tx(&wallet2, &seq_addr, spam2, enable_replaces);
                println!("Node 3 => Spamming {spam3} transactions to address {seq_addr}");
                let txids3 = send_spam_tx(&wallet3, &seq_addr, spam3, enable_replaces);
                (txids2, txids3)
            };
            if enable_replaces {
                bump_spam_txs(&wallet2, "Node 2", &txids2, replaces_per_miner);
                bump_spam_txs(&wallet3, "Node 3", &txids3, replaces_per_miner);
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}
