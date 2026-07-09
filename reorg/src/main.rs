use bitcoincore_rpc::{
    bitcoin::{address::NetworkUnchecked, Address, Amount, Network, Txid},
    jsonrpc, Client, RpcApi,
};
use serde_json::json;
use std::{
    collections::HashSet,
    env, process, thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Simchain reorg simulator.
//
// Forces a chain reorganization by invalidating the last N blocks on one
// miner node and mining N+1 replacements, one more than were invalidated,
// so the new chain is strictly longer and every node in the network reorgs
// to it. invalidateblock returns the orphaned blocks' transactions to the
// mempool; the replacement blocks are then filled by re-reading the mempool
// live and mining slices of it with generateblock, like the winning chain of
// a real reorg. Reading the mempool fresh for each block means RBF
// replacements that arrive mid-reorg are picked up automatically instead of
// leaving the block referencing an evicted txid. REORG_ADDS_NEW_TXS fresh
// transactions are seeded from the reorg node's wallet (REORG_WALLET_NAME)
// first, modelling a node that saw transactions its peers have not yet.
//
// Passing `empty` (or `--empty`) on the command line mines empty replacement
// blocks instead, leaving the orphaned txs unconfirmed in the mempool -- a
// chaos reorg, chosen per run rather than through a persistent setting.
//
// After mining the replacements, a witness node (REORG_WITNESS_NODE, "none"
// disables) is polled until it adopts the new chain; if the mining
// controller kept extending the old chain in the meantime, extra blocks are
// mined until the new chain wins network-wide.
//
// Modes (REORG_MODE):
//   once (default) - one reorg and exit. Depth: argv[1], or REORG_DEPTH, or 3.
//   auto           - every AUTO_REORG_EVERY_BLOCKS new blocks, reorg
//                    REORG_DEPTH blocks. Requires EVERY > DEPTH.

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
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
fn last_blocks(
    node: &Client,
    count: u64,
) -> Result<Vec<(u64, String, usize)>, bitcoincore_rpc::Error> {
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

/// `blocks` comes tip-first (highest height at index 0); print oldest-first so
/// the newest block is always on the last line, like ordered shell output.
fn print_blocks(blocks: &[(u64, String, usize)]) {
    for (height, hash, txs) in blocks.iter().rev() {
        println!("{height} : {txs:>3} txs -> {hash}");
    }
}

/// Send `count` fresh transactions from a wallet on the reorg node into its
/// own mempool, modelling a node that received transactions its peers have not
/// yet seen (clients broadcasting only to it). The live-mempool sweep then
/// mines them into the winning chain alongside the returned txs. Prefers
/// REORG_WALLET_NAME (the wallet the controller created on the reorg node);
/// falls back to the first loaded wallet if that one is not loaded.
fn inject_transactions(rpc_url: &str, rpc_user: &str, rpc_pass: &str, node: &Client, count: u64) {
    let preferred = env_or("REORG_WALLET_NAME", "node3");
    let wallet_name = match node.list_wallets() {
        Ok(wallets) if wallets.contains(&preferred) => preferred,
        Ok(wallets) if !wallets.is_empty() => {
            println!(
                "Wallet '{preferred}' not loaded on the reorg node, using '{}' instead",
                wallets[0]
            );
            wallets[0].clone()
        }
        _ => {
            println!("No wallet loaded on the reorg node, cannot add new transactions");
            return;
        }
    };
    let wallet = create_client(
        &format!("{rpc_url}/wallet/{wallet_name}"),
        rpc_user,
        rpc_pass,
    );
    let address = match wallet.get_new_address(None, None) {
        Ok(a) => match a.require_network(Network::Regtest) {
            Ok(a) => a,
            Err(e) => {
                println!("Wallet address not usable ({e}), skipping tx injection");
                return;
            }
        },
        Err(e) => {
            println!(
                "Could not get an address from wallet '{wallet_name}' ({e}), skipping tx injection"
            );
            return;
        }
    };
    let mut sent = 0;
    for _ in 0..count {
        match wallet.send_to_address(
            &address,
            Amount::from_sat(1000),
            None,
            None,
            None,
            None,
            None,
            None,
        ) {
            Ok(_) => sent += 1,
            Err(e) => {
                println!("Tx injection stopped after {sent} txs: {e}");
                break;
            }
        }
    }
    if sent > 0 {
        println!("Added {sent} new transactions from wallet '{wallet_name}' (txs this node saw first) to mine into the winning chain");
    } else {
        println!("Could not add new transactions (wallet '{wallet_name}' has no spendable funds)");
    }
}

/// The mining controller may extend the old chain on the other miner while
/// the replacements are being mined; if it lands a block, depth+1 new blocks
/// only tie and the network never reorgs. Poll a witness node until it adopts
/// the reorg node's tip, mining one extra block per round to outpace the old
/// chain. Gives up (with a warning) after `max_extra` extra blocks. In
/// `empty_mode` the extra blocks are empty too, so a chaos reorg does not
/// quietly confirm the orphaned txs through its race-winning block.
fn ensure_network_adopts(
    node: &Client,
    witness: &Client,
    witness_name: &str,
    mine_address: &Address,
    max_extra: u64,
    empty_mode: bool,
) -> Result<(), bitcoincore_rpc::Error> {
    for extra in 0..=max_extra {
        let tip = node.get_best_block_hash()?;
        // Give the new chain a moment to propagate before mining more.
        for _ in 0..12 {
            match witness.get_best_block_hash() {
                Ok(hash) if hash == tip => {
                    if extra > 0 {
                        println!("Network adopted the new chain after {extra} extra block(s)");
                    }
                    return Ok(());
                }
                Ok(_) => thread::sleep(Duration::from_millis(250)),
                Err(e) => {
                    println!("Witness node '{witness_name}' unreachable ({e}), cannot verify the network reorged");
                    return Ok(());
                }
            }
        }
        if extra == max_extra {
            break;
        }
        println!("'{witness_name}' is still on the old chain (miners kept extending it), mining 1 extra block...");
        if empty_mode {
            mine_exact(node, mine_address, &[])?;
        } else {
            node.generate_to_address(1, mine_address)?;
        }
    }
    println!("WARNING: the network did not adopt the new chain after {max_extra} extra blocks");
    Ok(())
}

/// Txids currently in the mempool, ordered parents-first (ascending ancestor
/// count). A leading slice of this list is always a valid set to mine into one
/// block: a child never precedes its parent, and every parent still in the
/// mempool sorts ahead of it.
fn live_mempool_topo(node: &Client) -> Result<Vec<Txid>, bitcoincore_rpc::Error> {
    let mut entries: Vec<(u64, Txid)> = Vec::new();
    for txid in node.get_raw_mempool()? {
        let ancestors = node
            .get_mempool_entry(&txid)
            .map(|e| e.ancestor_count)
            .unwrap_or(0);
        entries.push((ancestors, txid));
    }
    entries.sort_by_key(|(ancestors, _)| *ancestors);
    Ok(entries.into_iter().map(|(_, txid)| txid).collect())
}

/// Mine exactly `txids` (plus the coinbase) into one block with `generateblock`,
/// which -- unlike `generate_to_address` -- never pulls the rest of the mempool
/// in, so mining one block can never strand a later block's transactions.
/// If a tx went invalid since it was selected (e.g. RBF-replaced mid-reorg),
/// re-filter to what is still in the mempool and retry once; if nothing valid
/// remains (or the rejection was not about a missing tx), mine a real empty
/// block. Never drains the mempool.
fn mine_exact(
    node: &Client,
    mine_address: &Address,
    txids: &[Txid],
) -> Result<(), bitcoincore_rpc::Error> {
    let list: Vec<String> = txids.iter().map(|t| t.to_string()).collect();
    match node.call::<serde_json::Value>(
        "generateblock",
        &[json!(mine_address.to_string()), json!(list)],
    ) {
        Ok(_) => return Ok(()),
        Err(e) => println!(
            "generateblock rejected {} tx(s) ({e}), re-filtering to the live mempool...",
            list.len()
        ),
    }

    let live: HashSet<Txid> = node.get_raw_mempool()?.into_iter().collect();
    let filtered: Vec<String> = txids
        .iter()
        .filter(|t| live.contains(*t))
        .map(|t| t.to_string())
        .collect();

    // Something salvageable is still in the mempool: retry with just those.
    // Otherwise (all evicted, or the rejection was not about a missing tx and
    // retrying the same list would only fail again) mine an empty block; the
    // untouched txs stay in the mempool for the next block's sweep.
    let dropped = list.len() - filtered.len();
    if !filtered.is_empty() && dropped > 0 {
        println!(
            "  dropped {dropped} stale tx(s), mining the remaining {}",
            filtered.len()
        );
        node.call::<serde_json::Value>(
            "generateblock",
            &[json!(mine_address.to_string()), json!(filtered)],
        )?;
    } else {
        println!(
            "  mining an empty block, {} tx(s) left for the next block",
            filtered.len()
        );
        node.call::<serde_json::Value>(
            "generateblock",
            &[json!(mine_address.to_string()), json!(Vec::<String>::new())],
        )?;
    }
    Ok(())
}

fn do_reorg(
    node: &Client,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    depth: u64,
    mine_address: &Address,
    adds_new_txs: u64,
    empty_mode: bool,
    witness: Option<(&Client, &str)>,
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

    // Count the txs the orphaned blocks returned to the mempool (for the
    // summary). The mining below reads the mempool live for each block, so RBF
    // replacements that arrive mid-reorg are handled without special-casing.
    let returned = node.get_raw_mempool()?.len();
    println!("{returned} transactions returned to the mempool from the orphaned blocks");

    // A replacement block with the same timestamp and coinbase as the
    // invalidated one hashes identically and is rejected as known-invalid,
    // so wait until the clock has moved past the original block's time.
    while now_secs() <= target_time {
        thread::sleep(Duration::from_millis(250));
    }

    let blocks_to_mine = depth + 1;
    if empty_mode {
        // Chaos reorg: mine empty replacement blocks and leave the orphaned txs
        // unconfirmed in the mempool, like a miner that reorgs with empty
        // blocks. adds_new_txs is ignored -- empty means empty.
        println!("Mining {blocks_to_mine} EMPTY replacement blocks (chaos reorg, one extra so the new chain wins network-wide)...");
        for _ in 0..blocks_to_mine {
            mine_exact(node, mine_address, &[])?;
        }
    } else {
        // Seed the mempool with brand-new txs this node "saw first" so the
        // winning chain carries them alongside the returned txs.
        if adds_new_txs > 0 {
            inject_transactions(rpc_url, rpc_user, rpc_pass, node, adds_new_txs);
        }

        // Re-mine the live mempool, spread evenly across the replacement blocks,
        // like the competing chain of a real reorg. Reading it fresh each round
        // reflects any RBF replacements; the last block's ceil takes whatever is
        // left, so no tx is stranded.
        println!("Mining {blocks_to_mine} replacement blocks from the live mempool (one extra so the new chain wins network-wide)...");
        for i in 0..blocks_to_mine as usize {
            let blocks_left = blocks_to_mine as usize - i;
            let live = live_mempool_topo(node)?;
            let take = ((live.len() + blocks_left - 1) / blocks_left).min(live.len());
            mine_exact(node, mine_address, &live[..take])?;
        }
    }

    // Make sure the rest of the network actually switched to the new chain
    // before declaring success (the controller may have kept mining the old
    // one), then let it propagate before reporting.
    if let Some((witness, witness_name)) = witness {
        ensure_network_adopts(node, witness, witness_name, mine_address, 10, empty_mode)?;
    }
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

    println!(
        "\n=== Reorg done: blocks from height {target_height} replaced, new tip {} ===",
        node.get_block_count()?
    );
    Ok(())
}

fn main() {
    let rpc_user = env_or("BTC_RPC_USER", "foo");
    let rpc_pass = env_or("BTC_RPC_PASS", "rpcpassword");
    let node_name = env_or("REORG_NODE", "btc-simnet-node3");
    let rpc_port = env_or("REORG_NODE_RPC_PORT", "18443");
    let mode = env_or("REORG_MODE", "once");
    let every: u64 = env_or("AUTO_REORG_EVERY_BLOCKS", "20")
        .parse()
        .expect("AUTO_REORG_EVERY_BLOCKS must be a positive integer");
    // Brand-new txs the reorg node mines into the winning chain, modelling a
    // node that received transactions its peers have not yet seen. Seeded into
    // the mempool before mining (0 disables). Ignored in empty mode.
    let adds_new_txs: u64 = env_or("REORG_ADDS_NEW_TXS", "5")
        .parse()
        .expect("REORG_ADDS_NEW_TXS must be a non-negative integer");

    // CLI arguments, order-independent, forwarded through simulate-reorg.sh:
    //   <depth>          the first bare number, else REORG_DEPTH
    //   empty | --empty  mine empty replacement blocks (chaos reorg) rather
    //                    than re-mining the orphaned txs. Chosen per run, not a
    //                    persistent setting, so a real reorg and an empty one
    //                    can be issued against the same running chain.
    let cli_args: Vec<String> = env::args().skip(1).collect();
    let empty_mode = cli_args.iter().any(|a| a == "empty" || a == "--empty");
    let depth: u64 = cli_args
        .iter()
        .find_map(|a| a.parse::<u64>().ok())
        .unwrap_or_else(|| {
            env_or("REORG_DEPTH", "3")
                .parse()
                .expect("REORG_DEPTH must be a positive integer")
        });
    if depth < 1 {
        eprintln!("Reorg depth must be at least 1");
        process::exit(1);
    }

    let mine_address: Address<NetworkUnchecked> = env_or(
        "REORG_MINE_ADDRESS",
        "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr",
    )
    .parse()
    .expect("Invalid REORG_MINE_ADDRESS");
    let mine_address = mine_address
        .require_network(Network::Regtest)
        .expect("REORG_MINE_ADDRESS must be a regtest address");

    let rpc_url = format!("http://{node_name}:{rpc_port}");
    let node = create_client(&rpc_url, &rpc_user, &rpc_pass);
    wait_for_node(&node, &node_name);

    // Witness node: another node polled after the reorg to confirm the whole
    // network adopted the new chain (node1 never mines, ideal witness).
    // REORG_WITNESS_NODE=none disables the check.
    let witness_name = env_or("REORG_WITNESS_NODE", "btc-simnet-node1");
    let witness_client;
    let witness: Option<(&Client, &str)> = if witness_name == "none" || witness_name == node_name {
        None
    } else {
        witness_client = create_client(
            &format!("http://{witness_name}:{rpc_port}"),
            &rpc_user,
            &rpc_pass,
        );
        Some((&witness_client, witness_name.as_str()))
    };

    match mode.as_str() {
        "once" => {
            if let Err(e) = do_reorg(
                &node,
                &rpc_url,
                &rpc_user,
                &rpc_pass,
                depth,
                &mine_address,
                adds_new_txs,
                empty_mode,
                witness,
            ) {
                eprintln!("Reorg failed: {e}");
                process::exit(1);
            }
        }
        "auto" => {
            if every <= depth {
                eprintln!(
                    "AUTO_REORG_EVERY_BLOCKS ({every}) must be greater than REORG_DEPTH ({depth})"
                );
                process::exit(1);
            }
            let mut last = node.get_block_count().expect("get_block_count failed");
            println!("Auto-reorg mode: every {every} blocks, reorg the last {depth} (current height {last})");
            loop {
                match node.get_block_count() {
                    Ok(tip) if tip >= last + every => {
                        if let Err(e) = do_reorg(
                            &node,
                            &rpc_url,
                            &rpc_user,
                            &rpc_pass,
                            depth,
                            &mine_address,
                            adds_new_txs,
                            empty_mode,
                            witness,
                        ) {
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
