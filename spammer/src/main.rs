mod common;
mod node_wallet_spammer;
mod raw_transaction_spammer;

use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use common::{burn_address, create_client, env_or, wait_for_funds, MINER_COUNT};
use raw_transaction_spammer::RawSpammer;
use std::{env, thread, time::Duration};

// Shared block-watch loop: whenever a new block appears, run one spam cycle
// (whatever the selected engine does) and report how long it took -- the
// number to compare against BLOCK_INTERVAL_SECS when tuning for full blocks.
fn run_block_loop(node1: &Client, mut cycle: impl FnMut() -> usize) {
    let mut spammed_at_block_height = 0;
    loop {
        let current_block_height = node1.get_block_count().unwrap();
        if current_block_height > spammed_at_block_height {
            spammed_at_block_height = current_block_height;
            let cycle_start = std::time::Instant::now();
            let accepted = cycle();
            println!(
                "Spam cycle done in {:.1}s ({accepted} txs accepted)",
                cycle_start.elapsed().as_secs_f32()
            );
        }
        thread::sleep(Duration::from_millis(200));
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

    // Which engine builds the spam: true (default) = raw engine, the spammer
    // signs its own transactions and the wallets are bypassed; false = node
    // wallet engine, spam goes through sendtoaddress/sendmany on the miner
    // wallets (the original behavior, kept selectable).
    let use_raw = matches!(env_or("USE_RAW_TX_SPAM", "true").as_str(), "true" | "1");

    // Total spam txs offered per block: the number a block explorer shows per
    // block (plus coinbase) as long as blocks are not already full. Splitting
    // it across the miner wallets is this tool's job, not the user's; the
    // legacy per-miner variable is still honored so old .env files keep working.
    let spam_txs_per_block: u64 = match env::var("SPAM_TXS_PER_BLOCK") {
        Ok(v) => v
            .parse()
            .expect("SPAM_TXS_PER_BLOCK must be a positive integer"),
        Err(_) => match env::var("SPAM_PER_MINER_PER_BLOCK") {
            Ok(v) => {
                let per_miner: u64 = v
                    .parse()
                    .expect("SPAM_PER_MINER_PER_BLOCK must be a positive integer");
                println!("WARNING: SPAM_PER_MINER_PER_BLOCK is deprecated, set SPAM_TXS_PER_BLOCK (total per block) instead; using {}", per_miner * MINER_COUNT);
                per_miner * MINER_COUNT
            }
            Err(_) => 100,
        },
    };
    // node2 takes the odd remainder so the two shares always sum to the total
    let spam2 = spam_txs_per_block.div_ceil(MINER_COUNT);
    let spam3 = spam_txs_per_block / MINER_COUNT;
    let fanout_utxos: u64 = env_or("SPAM_FANOUT_UTXOS", "50")
        .parse()
        .expect("SPAM_FANOUT_UTXOS must be a positive integer");
    // 0 = sequential mode: one tx with a single burn output at a time, so txs
    // reach the mempool one by one like p2p traffic on a real network. N > 0
    // = batch mode: each spam tx carries N burn outputs (the shape of real
    // exchange-payout traffic) -- the way to FILL blocks on short intervals
    // (see SETTINGS.md "Full blocks" for ready-made values).
    let sendmany_outputs: u64 = env_or("SPAM_SENDMANY_OUTPUTS", "0")
        .parse()
        .expect("SPAM_SENDMANY_OUTPUTS must be a non-negative integer");
    // RBF traffic: when enabled ("true" or "1") every spam tx signals BIP125
    // and the newest few of each batch get fee-bumped right after sending.
    let enable_replaces = matches!(
        env_or("ENABLE_SPAM_REPLACES", "false").as_str(),
        "true" | "1"
    );
    let replaces_per_miner: u64 = env_or("SPAM_REPLACES_PER_MINER_PER_BLOCK", "5")
        .parse()
        .expect("SPAM_REPLACES_PER_MINER_PER_BLOCK must be a non-negative integer");
    // FALLBACK_FEE is what wallet-engine spam ends up paying (the wallet
    // estimator has no data and falls back to it), so the raw engine pays
    // exactly the same rate: one knob sets the simnet's fee price level for
    // both engines. Same units as the node flag, BTC/kvB.
    let fallback_fee: f64 = env_or("FALLBACK_FEE", "0.0001")
        .parse()
        .expect("FALLBACK_FEE must be a number (BTC/kvB)");
    let fee_rate_sat_vb = fallback_fee * 100_000.0;
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
    let wallet2 = create_client(
        &format!("{node2_url}/wallet/{wallet2_name}"),
        &rpc_user,
        &rpc_pass,
    );
    let wallet3 = create_client(
        &format!("{node3_url}/wallet/{wallet3_name}"),
        &rpc_user,
        &rpc_pass,
    );

    wait_for_funds(&wallet2, &wallet2_name);
    wait_for_funds(&wallet3, &wallet3_name);

    if use_raw {
        // Raw engine: one instance per miner node, each with its own key and
        // UTXO pool, submitting to its own node -- the same two independent
        // RPC pipelines the wallet engine gets from its two wallets. The
        // wallet clients are only kept for funding pulls.
        println!(
            "Spam engine: raw transactions (USE_RAW_TX_SPAM=true), paying {fee_rate_sat_vb} sat/vB"
        );
        let mut engine2 = RawSpammer::new(
            create_client(&node2_url, &rpc_user, &rpc_pass),
            wallet2,
            &wallet2_name,
            "Node 2",
            fee_rate_sat_vb,
            sendmany_outputs,
        );
        let mut engine3 = RawSpammer::new(
            create_client(&node3_url, &rpc_user, &rpc_pass),
            wallet3,
            &wallet3_name,
            "Node 3",
            fee_rate_sat_vb,
            sendmany_outputs,
        );
        // The raw engine always needs a branch pool (a single UTXO caps the
        // whole engine at one 25-tx unconfirmed chain), so 0 means 1 branch
        // rather than "disabled" like the wallet engine's fan-out.
        let fanout_target = fanout_utxos.max(1);
        let need2 = spam2.min(fanout_target);
        let need3 = spam3.min(fanout_target);
        run_block_loop(&node1, move || {
            let (txids2, txids3) = thread::scope(|s| {
                let t2 = s.spawn(|| {
                    engine2.spam_round(
                        spam2,
                        need2,
                        fanout_target,
                        enable_replaces,
                        replaces_per_miner,
                    )
                });
                let t3 = s.spawn(|| {
                    engine3.spam_round(
                        spam3,
                        need3,
                        fanout_target,
                        enable_replaces,
                        replaces_per_miner,
                    )
                });
                (
                    t2.join().expect("node2 spam thread panicked"),
                    t3.join().expect("node3 spam thread panicked"),
                )
            });
            txids2.len() + txids3.len()
        });
    } else {
        println!("Spam engine: node wallets (USE_RAW_TX_SPAM=false)");
        // Sequential mode target: one shared burn address -- reusing a single
        // address is exactly what real dust spam looks like.
        let seq_addr = burn_address(0);

        // Batch mode address pool: one fixed set of burn addresses, generated once
        // and shared by both miners' sendmany calls (the keys only need to be
        // distinct within one tx). Empty (and unused) in sequential mode.
        let batch_addrs: Vec<Address> = (1..=sendmany_outputs).map(burn_address).collect();

        // Cover a block's spam, but never require more branches than we fan out to.
        let fanout_need = spam2.min(fanout_utxos);

        run_block_loop(&node1, move || {
            // One thread per wallet: fan-out top-up, this block's spam and
            // the wallet's own RBF bumps, both wallets working their own
            // node at the same time.
            let (txids2, txids3) = thread::scope(|s| {
                let t2 = s.spawn(|| {
                    node_wallet_spammer::spam_round(
                        &wallet2,
                        &wallet2_name,
                        "Node 2",
                        spam2,
                        fanout_need,
                        fanout_utxos,
                        &seq_addr,
                        &batch_addrs,
                        enable_replaces,
                        replaces_per_miner,
                    )
                });
                let t3 = s.spawn(|| {
                    node_wallet_spammer::spam_round(
                        &wallet3,
                        &wallet3_name,
                        "Node 3",
                        spam3,
                        fanout_need,
                        fanout_utxos,
                        &seq_addr,
                        &batch_addrs,
                        enable_replaces,
                        replaces_per_miner,
                    )
                });
                (
                    t2.join().expect("node2 spam thread panicked"),
                    t3.join().expect("node3 spam thread panicked"),
                )
            });
            txids2.len() + txids3.len()
        });
    }
}
