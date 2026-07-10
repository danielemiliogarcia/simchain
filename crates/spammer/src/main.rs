mod common;
mod node_wallet_spammer;
mod raw_transaction_spammer;

use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use common::{
    burn_address, create_client, create_jsonrpc_client, env_or, rpc_retry, wait_for_funds,
    MINER_COUNT,
};
use raw_transaction_spammer::RawSpammer;
use std::{env, thread, time::Duration};

// Shared block-watch loop: whenever a new block appears, run one spam cycle
// (whatever the selected engine does) and report how long it took -- the
// number to compare against BLOCK_INTERVAL_MEAN_SECS when tuning for full blocks.
fn run_block_loop(node1: &Client, mut cycle: impl FnMut() -> usize) {
    let mut spammed_at_block_height = 0;
    loop {
        let current_block_height = rpc_retry("get node1 block count", || node1.get_block_count());
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

    // Fixed tx count for the OUTPUT spam modes (sequential/batch) and the
    // wallet engine. In DATA/HYBRID mode the fill is driven by
    // SPAM_FILL_BLOCK_RATIO instead and this is ignored. Renamed from
    // SPAM_TXS_PER_BLOCK (still honored, as is the older per-miner variable),
    // so no existing .env breaks.
    let fixed_txs_per_block: u64 = match env::var("SPAM_FIXED_TXS_PER_BLOCK")
        .or_else(|_| env::var("SPAM_TXS_PER_BLOCK"))
    {
        Ok(v) => v
            .parse()
            .expect("SPAM_FIXED_TXS_PER_BLOCK must be a positive integer"),
        Err(_) => match env::var("SPAM_PER_MINER_PER_BLOCK") {
            Ok(v) => {
                let per_miner: u64 = v
                    .parse()
                    .expect("SPAM_PER_MINER_PER_BLOCK must be a positive integer");
                println!("WARNING: SPAM_PER_MINER_PER_BLOCK is deprecated, set SPAM_FIXED_TXS_PER_BLOCK (total per block) instead; using {}", per_miner * MINER_COUNT);
                per_miner * MINER_COUNT
            }
            Err(_) => 100,
        },
    };
    // node2 takes the odd remainder so the two shares always sum to the total
    let fixed2 = fixed_txs_per_block.div_ceil(MINER_COUNT);
    let fixed3 = fixed_txs_per_block / MINER_COUNT;
    let fanout_utxos: u64 = env_or("SPAM_FANOUT_UTXOS", "50")
        .parse()
        .expect("SPAM_FANOUT_UTXOS must be a positive integer");
    // OUTPUT-mode fatness: 0 = sequential (one burn output per tx, p2p-like
    // arrival), N > 0 = batch (N burn outputs per tx, exchange-payout-shaped).
    // Ignored in DATA/HYBRID mode. (See SETTINGS.md "Full blocks".)
    let sendmany_outputs: u64 = env_or("SPAM_SENDMANY_OUTPUTS", "0")
        .parse()
        .expect("SPAM_SENDMANY_OUTPUTS must be a non-negative integer");
    // DATA/HYBRID mode (raw engine), the default: SPAM_TX_DATA_MAX_BYTES > 0
    // fills blocks with OP_RETURN data txs (no UTXO-set growth, a handful
    // fill a block). Each tx's payload is drawn log-uniformly in [MIN, MAX];
    // MIN = 0 (or >= MAX) makes every data tx exactly MAX. Capped just under
    // the 100k vB standard-tx limit. Needs Core 30+ (the compose default
    // image). Set 0 for the legacy OUTPUT mode (burn-output txs, UTXO-heavy).
    // Renamed from SPAM_TX_DATA_BYTES (still honored).
    const MAX_DATA_BYTES: u64 = 98_000;
    let data_max_bytes: u64 = {
        let requested: u64 = env::var("SPAM_TX_DATA_MAX_BYTES")
            .or_else(|_| env::var("SPAM_TX_DATA_BYTES"))
            .unwrap_or_else(|_| "90000".to_string())
            .parse()
            .expect("SPAM_TX_DATA_MAX_BYTES must be a non-negative integer");
        if requested > MAX_DATA_BYTES {
            println!("WARNING: SPAM_TX_DATA_MAX_BYTES={requested} exceeds the {MAX_DATA_BYTES}-byte standard-tx limit, clamping to {MAX_DATA_BYTES}");
            MAX_DATA_BYTES
        } else {
            requested
        }
    };
    // Bottom of the data-size range. 0 or >= MAX means uniform txs of
    // exactly MAX; a value below MAX (default 250) spreads sizes
    // log-uniformly for a realistic mix of tx sizes. Clamped to MAX.
    let data_min_bytes: u64 = env_or("SPAM_TX_DATA_MIN_BYTES", "250")
        .parse::<u64>()
        .expect("SPAM_TX_DATA_MIN_BYTES must be a non-negative integer")
        .min(data_max_bytes);
    // HYBRID small txs: this many minimum-size (~140 vB) P2WPKH floor-priced
    // txs per block, cosmetic small-payment-shaped traffic on top of the data
    // fill. NOT the fee floor -- the airtight floor is SPAM_FLOOR_POOL_TXS
    // below. 0 disables.
    let small_txs_per_block: u64 = env_or("SPAM_SMALL_TXS_PER_BLOCK", "0")
        .parse()
        .expect("SPAM_SMALL_TXS_PER_BLOCK must be a non-negative integer");
    // DATA/HYBRID fill target, measured in blocks of mempool weight: 0.5 =
    // half-full blocks (floor has no effect), 1 = full blocks + a shallow
    // backlog, 5 = full blocks + ~4 pending blocks visible in the mempool.
    // Default 2: the mempool oscillates ~1 block around the target between
    // top-ups, so 2 keeps a full block of floor-priced supply at every
    // template and the fee floor stays airtight; 1 rides the trough and can
    // leave the occasional partial block (floor leaks that block).
    let fill_block_ratio: f64 = env_or("SPAM_FILL_BLOCK_RATIO", "2.0")
        .parse()
        .expect("SPAM_FILL_BLOCK_RATIO must be a number");
    // Airtight fee floor (raw DATA/HYBRID only): keep this many standalone
    // floor-priced minimum-size fill txs STANDING in the mempool at all times,
    // split across the miners. Each fill spends a confirmed UTXO from a
    // dedicated pool (never unconfirmed change), so a below-floor tx has
    // nowhere left to slip in. 0 disables (the floor is then soft).
    let floor_pool_txs: u64 = env_or("SPAM_FLOOR_POOL_TXS", "4000")
        .parse()
        .expect("SPAM_FLOOR_POOL_TXS must be a non-negative integer");
    // Whether to auto-derive the branch pool from the fill ratio. true
    // (default): use max(12, ceil(ratio x 15)) branches for headroom, ignoring
    // SPAM_FANOUT_UTXOS. false: use SPAM_FANOUT_UTXOS, hard-erroring if it is
    // below the ratio x 10 minimum needed to hold that many blocks unconfirmed.
    let fanout_auto = matches!(env_or("SPAM_FANOUT_AUTO", "true").as_str(), "true" | "1");
    // RBF traffic: when enabled ("true" or "1") every spam tx signals BIP125
    // and the newest few of each batch get fee-bumped right after sending.
    let enable_replaces = matches!(
        env_or("ENABLE_SPAM_REPLACES", "false").as_str(),
        "true" | "1"
    );
    let replaces_per_miner: u64 = env_or("SPAM_REPLACES_PER_MINER_PER_BLOCK", "5")
        .parse()
        .expect("SPAM_REPLACES_PER_MINER_PER_BLOCK must be a non-negative integer");
    // FALLBACK_FEE is the simulated floor level. Floor fills pay exactly this;
    // DATA/HYBRID bulk spam pays a tiny premium so miners drain bulk first and
    // keep the floor fills for residual gaps. Same units as the node flag,
    // BTC/kvB.
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
        // UTXO pool. Floor-fill txs are accepted by their owner node, then
        // relayed by RPC to the other miner so both rotating miners can
        // template from a fresh local floor pool without waiting on P2P
        // propagation. Bulk DATA txs stay on the owner-node path.
        let mut engine2 = RawSpammer::new(
            create_client(&node2_url, &rpc_user, &rpc_pass),
            create_jsonrpc_client(&node2_url, &rpc_user, &rpc_pass),
            vec![create_jsonrpc_client(&node3_url, &rpc_user, &rpc_pass)],
            wallet2,
            &wallet2_name,
            "Node 2",
            fee_rate_sat_vb,
            sendmany_outputs,
            data_min_bytes,
            data_max_bytes,
        );
        let mut engine3 = RawSpammer::new(
            create_client(&node3_url, &rpc_user, &rpc_pass),
            create_jsonrpc_client(&node3_url, &rpc_user, &rpc_pass),
            vec![create_jsonrpc_client(&node2_url, &rpc_user, &rpc_pass)],
            wallet3,
            &wallet3_name,
            "Node 3",
            fee_rate_sat_vb,
            sendmany_outputs,
            data_min_bytes,
            data_max_bytes,
        );

        if data_max_bytes > 0 {
            // DATA/HYBRID mode: fill the mempool to SPAM_FILL_BLOCK_RATIO
            // blocks of weight each block, measured live, with varied-size
            // OP_RETURN data txs plus a guaranteed batch of gap-sealer txs.
            //
            // The branch pool must hold R blocks of unconfirmed spam, and each
            // branch chain caps at ~101k vB, so it needs >= R x 10 branches.
            let required_min = std::cmp::max(12, (fill_block_ratio * 10.0).ceil() as u64);
            let effective_fanout = if fanout_auto {
                let f = std::cmp::max(12, (fill_block_ratio * 15.0).ceil() as u64);
                println!("Raw DATA/HYBRID mode: fanout auto-derived to {f} branches (SPAM_FILL_BLOCK_RATIO={fill_block_ratio} x15, min 12)");
                f
            } else {
                assert!(
                    fanout_utxos >= required_min,
                    "SPAM_FANOUT_UTXOS={fanout_utxos} is too low for SPAM_FILL_BLOCK_RATIO={fill_block_ratio}: need >= {required_min} branches (ratio x10) to hold that many blocks of unconfirmed spam, or the mempool cannot reach the target and blocks come out partial. Raise SPAM_FANOUT_UTXOS to >= {required_min}, or set SPAM_FANOUT_AUTO=true."
                );
                println!("Raw DATA/HYBRID mode: fanout manual {fanout_utxos} branches (SPAM_FANOUT_AUTO=false)");
                fanout_utxos
            };
            if fill_block_ratio < 1.0 && (fallback_fee - 0.0001).abs() > 1e-9 {
                println!(
                    "WARNING: SPAM_FILL_BLOCK_RATIO={fill_block_ratio} < 1 leaves blocks only ~{:.0}% full, so the raised FALLBACK_FEE floor will NOT hold -- cheaper txs still confirm in the unused block space, and the floor fill pool cannot seal deliberately partial blocks (expected if you are simulating an uncongested chain).",
                    fill_block_ratio * 100.0
                );
            }
            let small2 = small_txs_per_block.div_ceil(MINER_COUNT);
            let small3 = small_txs_per_block / MINER_COUNT;
            // Each engine keeps its share of the standing floor fills on its
            // OWN node, so both miners always have the fills locally when
            // they assemble a block template.
            let pool2 = floor_pool_txs.div_ceil(MINER_COUNT);
            let pool3 = floor_pool_txs / MINER_COUNT;
            // A full block is 4M WU = 1M vB; getmempoolinfo's `bytes` is the
            // mempool's total vsize, in the same units.
            const BLOCK_VSIZE: u64 = 1_000_000;
            let meter = create_client(&node1_url, &rpc_user, &rpc_pass);
            println!(
                "Spam engine: raw DATA/HYBRID mode, {data_min_bytes}..{data_max_bytes} byte OP_RETURN, {small_txs_per_block} gap-sealers/block, {floor_pool_txs} standing 110-vB floor fills, fill {fill_block_ratio} block(s), floor {fee_rate_sat_vb} sat/vB"
            );
            run_block_loop(&node1, move || {
                // Measure the live mempool right after the new block drained it,
                // and top it back up to R blocks (plus a small reserve at R>=1
                // so packing lands the block full). At R<1 the target is below
                // one block, so blocks come out partial by design.
                let mempool = meter
                    .get_mempool_info()
                    .map(|m| m.bytes as u64)
                    .unwrap_or(0);
                let reserve = if fill_block_ratio >= 1.0 {
                    BLOCK_VSIZE / 10
                } else {
                    0
                };
                let target = (fill_block_ratio * BLOCK_VSIZE as f64) as u64 + reserve;
                let deficit = target.saturating_sub(mempool);
                let d2 = deficit / MINER_COUNT;
                let d3 = deficit - d2;
                let (r2, r3) = thread::scope(|s| {
                    // Floor fills first: the standing pool is the airtight
                    // guarantee, the data fill is the bulk behind it.
                    let t2 = s.spawn(|| {
                        let fills = engine2.floor_round(pool2);
                        let (txids, _) = engine2.hybrid_round(
                            d2,
                            small2,
                            effective_fanout,
                            enable_replaces,
                            replaces_per_miner,
                        );
                        fills + txids.len()
                    });
                    let t3 = s.spawn(|| {
                        let fills = engine3.floor_round(pool3);
                        let (txids, _) = engine3.hybrid_round(
                            d3,
                            small3,
                            effective_fanout,
                            enable_replaces,
                            replaces_per_miner,
                        );
                        fills + txids.len()
                    });
                    (
                        t2.join().expect("node2 spam thread panicked"),
                        t3.join().expect("node3 spam thread panicked"),
                    )
                });
                r2 + r3
            });
        } else {
            // OUTPUT mode: a fixed count of burn-output txs per block.
            println!(
                "Spam engine: raw transactions (USE_RAW_TX_SPAM=true), OUTPUT mode, {fee_rate_sat_vb} sat/vB"
            );
            if floor_pool_txs > 0 {
                println!(
                    "NOTE: SPAM_FLOOR_POOL_TXS only applies to DATA/HYBRID mode (SPAM_TX_DATA_MAX_BYTES > 0); no floor fill pool in OUTPUT mode"
                );
            }
            // The raw engine always needs a branch pool (a single UTXO caps the
            // whole engine at one 25-tx unconfirmed chain), so 0 means 1 branch.
            let fanout_target = fanout_utxos.max(1);
            run_block_loop(&node1, move || {
                let (txids2, txids3) = thread::scope(|s| {
                    let t2 = s.spawn(|| {
                        engine2.output_round(
                            fixed2,
                            fanout_target,
                            enable_replaces,
                            replaces_per_miner,
                        )
                    });
                    let t3 = s.spawn(|| {
                        engine3.output_round(
                            fixed3,
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
        }
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
        let fanout_need = fixed2.min(fanout_utxos);

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
                        fixed2,
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
                        fixed3,
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
