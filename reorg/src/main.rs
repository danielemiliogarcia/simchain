use bitcoincore_rpc::{
    bitcoin::{address::NetworkUnchecked, Address, Amount, Network, Txid},
    jsonrpc, Client, RpcApi,
};
use serde_json::json;
use std::{
    env, process, thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Simchain reorg simulator.
//
// Forces a chain reorganization by invalidating the last N blocks on one
// miner node and mining N+1 replacements, one more than were invalidated,
// so the new chain is strictly longer and every node in the network reorgs
// to it. invalidateblock returns the orphaned blocks' transactions to the
// mempool, so the replacement blocks carry the same real transactions, like
// the winning chain of a real reorg. If the mempool ends up empty (orphaned
// blocks had no txs), REORG_INJECT_TXS fresh transactions are sent from the
// reorg node's wallet before each empty replacement block so they are not
// empty.
//
// Modes (REORG_MODE):
//   once (default) - one reorg and exit. Depth: argv[1], or REORG_DEPTH, or 3.
//   auto           - every AUTO_REORG_EVERY_BLOCKS new blocks, reorg
//                    REORG_DEPTH blocks. Requires EVERY > DEPTH.

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn create_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> Client {
    // Disconnecting blocks with hundreds of txs can exceed the default 15s
    // RPC timeout, so build the transport with a generous one.
    let (user, pass) = (rpc_user.to_string(), Some(rpc_pass.to_string()));
    let transport = jsonrpc::simple_http::SimpleHttpTransport::builder()
        .url(rpc_url)
        .expect("invalid RPC url")
        .auth(user, pass)
        .timeout(Duration::from_secs(300))
        .build();
    Client::from_jsonrpc(jsonrpc::client::Client::with_transport(transport))
}

fn wait_for_node(node: &Client, name: &str) {
    loop {
        match node.get_block_count() {
            Ok(_) => return,
            Err(_) => {
                println!("Waiting for {name} RPC...");
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// (height, hash, tx count) for the last `count` blocks, tip first.
fn last_blocks(node: &Client, count: u64) -> Result<Vec<(u64, String, usize)>, bitcoincore_rpc::Error> {
    let tip = node.get_block_count()?;
    let mut blocks = Vec::new();
    for i in 0..count {
        if i > tip {
            break;
        }
        let height = tip - i;
        let hash = node.get_block_hash(height)?;
        let info = node.get_block_info(&hash)?;
        blocks.push((height, hash.to_string(), info.tx.len()));
    }
    Ok(blocks)
}

fn print_blocks(blocks: &[(u64, String, usize)]) {
    for (height, hash, txs) in blocks {
        println!("{height} : {txs:>3} txs -> {hash}");
    }
}

/// If a wallet is loaded on the reorg node, send `count` transactions to
/// itself so the replacement blocks are not empty.
fn inject_transactions(rpc_url: &str, rpc_user: &str, rpc_pass: &str, node: &Client, count: u64) {
    let wallet_name = match node.list_wallets() {
        Ok(wallets) if !wallets.is_empty() => wallets[0].clone(),
        _ => {
            println!("No wallet loaded on the reorg node, replacement blocks may be empty");
            return;
        }
    };
    let wallet = create_client(&format!("{rpc_url}/wallet/{wallet_name}"), rpc_user, rpc_pass);
    let address = match wallet.get_new_address(None, None) {
        Ok(a) => match a.require_network(Network::Regtest) {
            Ok(a) => a,
            Err(e) => {
                println!("Wallet address not usable ({e}), skipping tx injection");
                return;
            }
        },
        Err(e) => {
            println!("Could not get an address from wallet '{wallet_name}' ({e}), skipping tx injection");
            return;
        }
    };
    let mut sent = 0;
    for _ in 0..count {
        match wallet.send_to_address(&address, Amount::from_sat(1000), None, None, None, None, None, None) {
            Ok(_) => sent += 1,
            Err(e) => {
                println!("Tx injection stopped after {sent} txs: {e}");
                break;
            }
        }
    }
    if sent > 0 {
        println!("Injected {sent} transactions from wallet '{wallet_name}' so replacement blocks are not empty");
    } else {
        println!("Could not inject transactions (wallet '{wallet_name}' has no spendable funds), replacement blocks may be empty");
    }
}

fn do_reorg(
    node: &Client,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    depth: u64,
    mine_address: &Address,
    inject_txs: u64,
) -> Result<(), bitcoincore_rpc::Error> {
    let tip = node.get_block_count()?;
    if tip < depth + 1 {
        println!("Chain too short (height {tip}) to reorg {depth} blocks, skipping");
        return Ok(());
    }

    println!("\n=== Simulating a reorg of the last {depth} blocks ===\n");
    println!("--- Last {} blocks BEFORE reorg ---", depth + 2);
    let before = last_blocks(node, depth + 2)?;
    print_blocks(&before);

    let target_height = tip - depth + 1;
    let target_hash = node.get_block_hash(target_height)?;
    let target_time = node.get_block_info(&target_hash)?.time as u64;

    println!("\nInvalidating block {target_height} ({target_hash})...");
    node.invalidate_block(&target_hash)?;

    // Order the returned txs topologically (parents first) so they can be
    // split across the replacement blocks, like the competing chain of a
    // real reorg would have mined them.
    let mut returned: Vec<(u64, Txid)> = Vec::new();
    for txid in node.get_raw_mempool()? {
        let ancestors = node.get_mempool_entry(&txid).map(|e| e.ancestor_count).unwrap_or(0);
        returned.push((ancestors, txid));
    }
    returned.sort_by_key(|(ancestors, _)| *ancestors);
    println!("{} transactions returned to the mempool from the orphaned blocks", returned.len());

    // A replacement block with the same timestamp and coinbase as the
    // invalidated one hashes identically and is rejected as known-invalid,
    // so wait until the clock has moved past the original block's time.
    while now_secs() <= target_time {
        thread::sleep(Duration::from_millis(250));
    }

    let blocks_to_mine = depth + 1;
    let chunk_size = (returned.len() + blocks_to_mine as usize - 1) / blocks_to_mine as usize;
    println!("Mining {blocks_to_mine} replacement blocks (one extra so the new chain wins network-wide)...");
    for i in 0..blocks_to_mine as usize {
        let chunk: Vec<String> = if chunk_size > 0 {
            returned
                .iter()
                .skip(i * chunk_size)
                .take(chunk_size)
                .map(|(_, txid)| txid.to_string())
                .collect()
        } else {
            Vec::new()
        };

        if !chunk.is_empty() {
            // Mine exactly this slice of the returned txs into one block.
            match node.call::<serde_json::Value>("generateblock", &[json!(mine_address.to_string()), json!(chunk)]) {
                Ok(_) => continue,
                Err(e) => println!("generateblock rejected the tx slice ({e}), mining from the mempool instead"),
            }
        } else if inject_txs > 0 && node.get_raw_mempool()?.is_empty() {
            // No orphaned txs left for this block: top the mempool up from
            // the wallet so the block is not empty.
            inject_transactions(rpc_url, rpc_user, rpc_pass, node, inject_txs);
        }

        let mut attempts = 0;
        loop {
            match node.generate_to_address(1, mine_address) {
                Ok(_) => break,
                Err(e) => {
                    attempts += 1;
                    if attempts >= 3 {
                        return Err(e);
                    }
                    println!("Block generation rejected ({e}), retrying in 1s...");
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    // Let the new chain propagate before reporting.
    thread::sleep(Duration::from_secs(2));

    println!("\n--- Last {} blocks AFTER reorg ---", depth + 3);
    let after = last_blocks(node, depth + 3)?;
    print_blocks(&after);

    println!("\n--- Replaced blocks ---");
    for (height, old_hash, old_txs) in before.iter().rev() {
        if let Some((_, new_hash, new_txs)) = after.iter().find(|(h, _, _)| h == height) {
            if new_hash != old_hash {
                println!("{height} : {old_hash} ({old_txs} txs) => {new_hash} ({new_txs} txs)");
            }
        }
    }

    println!("\n=== Reorg done: blocks from height {target_height} replaced, new tip {} ===", node.get_block_count()?);
    Ok(())
}

fn main() {
    let rpc_user = env_or("BTC_RPC_USER", "foo");
    let rpc_pass = env_or("BTC_RPC_PASS", "rpcpassword");
    let node_name = env_or("REORG_NODE", "btc-simnet-node3");
    let rpc_port = env_or("REORG_NODE_RPC_PORT", "18443");
    let mode = env_or("REORG_MODE", "once");
    let every: u64 = env_or("AUTO_REORG_EVERY_BLOCKS", "20").parse().expect("AUTO_REORG_EVERY_BLOCKS must be a positive integer");
    let inject_txs: u64 = env_or("REORG_INJECT_TXS", "5").parse().expect("REORG_INJECT_TXS must be a non-negative integer");

    let depth: u64 = env::args()
        .nth(1)
        .unwrap_or_else(|| env_or("REORG_DEPTH", "3"))
        .parse()
        .expect("reorg depth must be a positive integer");
    if depth < 1 {
        eprintln!("Reorg depth must be at least 1");
        process::exit(1);
    }

    let mine_address: Address<NetworkUnchecked> = env_or("REORG_MINE_ADDRESS", "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr")
        .parse()
        .expect("Invalid REORG_MINE_ADDRESS");
    let mine_address = mine_address.require_network(Network::Regtest).expect("REORG_MINE_ADDRESS must be a regtest address");

    let rpc_url = format!("http://{node_name}:{rpc_port}");
    let node = create_client(&rpc_url, &rpc_user, &rpc_pass);
    wait_for_node(&node, &node_name);

    match mode.as_str() {
        "once" => {
            if let Err(e) = do_reorg(&node, &rpc_url, &rpc_user, &rpc_pass, depth, &mine_address, inject_txs) {
                eprintln!("Reorg failed: {e}");
                process::exit(1);
            }
        }
        "auto" => {
            if every <= depth {
                eprintln!("AUTO_REORG_EVERY_BLOCKS ({every}) must be greater than REORG_DEPTH ({depth})");
                process::exit(1);
            }
            let mut last = node.get_block_count().expect("get_block_count failed");
            println!("Auto-reorg mode: every {every} blocks, reorg the last {depth} (current height {last})");
            loop {
                match node.get_block_count() {
                    Ok(tip) if tip >= last + every => {
                        if let Err(e) = do_reorg(&node, &rpc_url, &rpc_user, &rpc_pass, depth, &mine_address, inject_txs) {
                            eprintln!("Reorg failed: {e}");
                        }
                        last = node.get_block_count().unwrap_or(tip);
                    }
                    Ok(_) => {}
                    Err(e) => println!("RPC error while polling height: {e}"),
                }
                thread::sleep(Duration::from_secs(2));
            }
        }
        other => {
            eprintln!("Unknown REORG_MODE '{other}' (expected: once | auto)");
            process::exit(1);
        }
    }
}
