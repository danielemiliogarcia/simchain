use bitcoincore_rpc::{bitcoin::{Address, Amount, Network}, Auth, Client, RpcApi};
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

// Split the wallet funds into `count` separate UTXOs. The mempool limits a
// chain of unconfirmed transactions to 25, so a wallet spending from a
// single UTXO can never place more than 25 txs per block. With `count`
// independent UTXOs the wallet can build `count` parallel chains and reach
// any realistic SPAM_PER_MINER_PER_BLOCK value.
fn fan_out(wallet: &Client, name: &str, count: u64) {
    let unspent = wallet.list_unspent(Some(1), None, None, None, None).unwrap();
    if unspent.len() as u64 >= count {
        println!("Wallet '{name}' already has {} UTXOs, no fan-out needed", unspent.len());
        return;
    }

    let trusted = wallet.get_balances().unwrap().mine.trusted.to_btc();
    // 0.1 BTC per branch funds years of dust spam; scale down if the wallet
    // is smaller than count * 0.1 (keep 20% margin for fees)
    let per_output = (trusted * 0.8 / count as f64).min(0.1);
    let per_output = (per_output * 1e8).floor() / 1e8;

    println!("Splitting wallet '{name}' funds into {count} UTXOs of {per_output} BTC each");
    let mut outputs = serde_json::Map::new();
    while outputs.len() < count as usize {
        let address = get_new_wallet_address(wallet);
        outputs.insert(address.to_string(), json!(per_output));
    }
    let txid: String = wallet.call("sendmany", &[json!(""), json!(outputs)]).unwrap();
    println!("Fan-out tx {txid} sent, waiting for it to confirm...");

    loop {
        let confirmed = wallet.list_unspent(Some(1), None, None, None, None).unwrap();
        if confirmed.len() as u64 >= count {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    println!("Wallet '{name}' fan-out confirmed");
}

// Send `count` txs and report how many actually made it, so empty blocks
// are noticed (a silent wallet error would defeat the spammer's purpose).
fn send_spam_tx(from: &Client, to_address: &Address, count: u64) -> u64 {
    // 546 sats is the dust limit for P2PKH outputs, the highest floor among
    // the common output types (bech32 is 294), so this amount is safely
    // above dust no matter what address type receives it.
    let amount = Amount::from_sat(546);
    let mut sent = 0;
    let mut first_error: Option<String> = None;
    for _ in 0..count {
        match from.send_to_address(&to_address, amount, None, None, None, None, None, None) {
            Ok(_) => sent += 1,
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
            }
        }
    }
    if let Some(error) = first_error {
        println!("WARNING: only {sent}/{count} spam txs accepted, first error: {error}");
    }
    sent
}

fn main() {
    let enable_spam = env_or("ENABLE_SPAM", "false") == "true";
    if !enable_spam {
        println!("ENABLE_SPAM is not 'true', nothing to do, exiting");
        return;
    }

    let spam_per_miner_per_block: u64 = env::var("SPAM_PER_MINER_PER_BLOCK").expect("SPAM_PER_MINER_PER_BLOCK missing").parse().unwrap();
    let fanout_utxos: u64 = env_or("SPAM_FANOUT_UTXOS", "50").parse().expect("SPAM_FANOUT_UTXOS must be a positive integer");
    let rpc_user = env::var("BTC_RPC_USER").expect("BTC_RPC_USER missing");
    let rpc_pass = env::var("BTC_RPC_PASS").expect("BTC_RPC_PASS missing");
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

    if fanout_utxos > 0 {
        fan_out(&wallet2, &wallet2_name, fanout_utxos);
        fan_out(&wallet3, &wallet3_name, fanout_utxos);
    }

    let addr2 = get_new_wallet_address(&wallet2);
    let addr3 = get_new_wallet_address(&wallet3);

    // In a loop, if a new block is detected, spam transactions
    let mut spammed_at_block_height = 0;
    loop {
        let current_block_height = node1.get_block_count().unwrap();
        if current_block_height > spammed_at_block_height {
            spammed_at_block_height = current_block_height;
            // spam transactions cross wallet
            println!("Node 2 => Spamming {spam_per_miner_per_block} transactions to address {addr3}");
            send_spam_tx(&wallet2, &addr3, spam_per_miner_per_block);
            println!("Node 3 => Spamming {spam_per_miner_per_block} transactions to address {addr2}");
            send_spam_tx(&wallet3, &addr2, spam_per_miner_per_block);
        }
        thread::sleep(Duration::from_millis(200));
    }
}
