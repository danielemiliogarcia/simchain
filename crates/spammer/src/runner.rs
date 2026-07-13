//! Spammer startup and engine coordination.

use crate::{
    burn::{burn_address, MINER_COUNT},
    config::SpamConfig,
    node_wallet_spammer,
    raw_transaction_spammer::RawSpammer,
    wallet::wait_for_funds,
};
use anyhow::Context;
use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use serde_json::json;
use simchain_common::{create_client, create_jsonrpc_client, create_wallet_client, rpc_retry};
use std::{thread, time::Duration};

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
            tracing::info!(
                "Spam cycle done in {:.1}s ({accepted} txs accepted)",
                cycle_start.elapsed().as_secs_f32()
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Connect the node and wallet clients, wait for mature funds, then start the
/// configured spam engine.
pub fn run() -> anyhow::Result<()> {
    if !SpamConfig::is_enabled() {
        tracing::info!("ENABLE_SPAM is not 'true', nothing to do, exiting");
        return Ok(());
    }

    let config = SpamConfig::global();
    let node1 = create_client(&config.node1_url)?;
    // Wallet-scoped clients keep working even if a user loads extra wallets on
    // a node (the generic RPC path breaks with more than one wallet).
    let wallet2 = create_wallet_client(&config.node2_url, &config.wallet2_name)?;
    let wallet3 = create_wallet_client(&config.node3_url, &config.wallet3_name)?;

    wait_for_funds(&wallet2, &config.wallet2_name);
    wait_for_funds(&wallet3, &config.wallet3_name);

    if config.use_raw {
        run_raw(&node1, wallet2, wallet3)
    } else {
        // The wallet engine sends without an explicit fee rate, so each
        // wallet's paytxfee decides what spam pays. Pin it to FALLBACK_FEE at
        // startup: a live retune of the fee floor then takes effect on a
        // spammer-only recreate, while the running nodes keep their old
        // -fallbackfee (paytxfee overrides the wallet fallback). A rejected
        // settxfee (e.g. below the node's minrelaytxfee) is a startup error,
        // so a bad retune crashes fast instead of silently keeping old fees.
        // `settxfee 0` is meaningful: it clears a previously persisted
        // wallet paytxfee and returns to estimation/fallback behavior.
        set_wallet_tx_fee(&wallet2, &config.wallet2_name, config.fallback_fee)?;
        set_wallet_tx_fee(&wallet3, &config.wallet3_name, config.fallback_fee)?;
        run_node_wallets(&node1, wallet2, wallet3);
        Ok(())
    }
}

fn set_wallet_tx_fee(wallet: &Client, name: &str, fee_btc_per_kvb: f64) -> anyhow::Result<()> {
    let accepted = wallet
        .call::<bool>("settxfee", &[json!(fee_btc_per_kvb)])
        .with_context(|| format!("settxfee {fee_btc_per_kvb} on wallet '{name}' failed"))?;
    anyhow::ensure!(
        accepted,
        "wallet '{name}' rejected settxfee {fee_btc_per_kvb}"
    );
    tracing::info!("Wallet '{name}' paytxfee pinned to {fee_btc_per_kvb} BTC/kvB (FALLBACK_FEE)");
    Ok(())
}

fn run_raw(node1: &Client, wallet2: Client, wallet3: Client) -> anyhow::Result<()> {
    let config = SpamConfig::global();
    // Raw engine: one instance per miner node, each with its own key and UTXO
    // pool. Floor fills are relayed to the other miner by RPC so both rotating
    // miners can template from a fresh local floor pool without waiting for P2P
    // propagation. Bulk DATA txs stay on their owner-node path.
    let mut engine2 = RawSpammer::new(
        create_client(&config.node2_url)?,
        create_jsonrpc_client(&config.node2_url)?,
        vec![create_jsonrpc_client(&config.node3_url)?],
        wallet2,
        &config.wallet2_name,
        "Node 2",
        config.fee_rate_sat_vb,
        config.sendmany_outputs,
        config.data_min_bytes,
        config.data_max_bytes,
    );
    let mut engine3 = RawSpammer::new(
        create_client(&config.node3_url)?,
        create_jsonrpc_client(&config.node3_url)?,
        vec![create_jsonrpc_client(&config.node2_url)?],
        wallet3,
        &config.wallet3_name,
        "Node 3",
        config.fee_rate_sat_vb,
        config.sendmany_outputs,
        config.data_min_bytes,
        config.data_max_bytes,
    );

    if config.data_max_bytes > 0 {
        run_raw_data_mode(node1, &mut engine2, &mut engine3)?;
    } else {
        run_raw_output_mode(node1, &mut engine2, &mut engine3);
    }
    Ok(())
}

fn run_raw_data_mode(
    node1: &Client,
    engine2: &mut RawSpammer,
    engine3: &mut RawSpammer,
) -> anyhow::Result<()> {
    let config = SpamConfig::global();
    // The branch pool must hold R blocks of unconfirmed spam, and each branch
    // chain caps at ~101k vB, so it needs at least R x 10 branches.
    let effective_fanout = if config.fanout_auto {
        let fanout = std::cmp::max(12, (config.fill_block_ratio * 15.0).ceil() as u64);
        tracing::info!("Raw DATA/HYBRID mode: fanout auto-derived to {fanout} branches (SPAM_FILL_BLOCK_RATIO={} x15, min 12)", config.fill_block_ratio);
        fanout
    } else {
        tracing::info!(
            "Raw DATA/HYBRID mode: fanout manual {} branches (SPAM_FANOUT_AUTO=false)",
            config.fanout_utxos
        );
        config.fanout_utxos
    };
    if config.fill_block_ratio < 1.0 && (config.fallback_fee - 0.0001).abs() > 1e-9 {
        tracing::warn!(
            "SPAM_FILL_BLOCK_RATIO={} < 1 leaves blocks only ~{:.0}% full, so the raised FALLBACK_FEE floor will NOT hold -- cheaper txs still confirm in the unused block space, and the floor fill pool cannot seal deliberately partial blocks (expected if you are simulating an uncongested chain).",
            config.fill_block_ratio,
            config.fill_block_ratio * 100.0
        );
    }
    let small2 = config.small_txs_per_block.div_ceil(MINER_COUNT);
    let small3 = config.small_txs_per_block / MINER_COUNT;
    // Each engine keeps its share of the standing floor fills on its OWN node,
    // so both miners have fills locally when assembling their block template.
    let pool2 = config.floor_pool_txs.div_ceil(MINER_COUNT);
    let pool3 = config.floor_pool_txs / MINER_COUNT;
    // A full block is 4M WU = 1M vB; getmempoolinfo's `bytes` has the same unit.
    const BLOCK_VSIZE: u64 = 1_000_000;
    let meter = create_client(&config.node1_url)?;
    tracing::info!(
        "Spam engine: raw DATA/HYBRID mode, {}..{} byte OP_RETURN, {} gap-sealers/block, {} standing 110-vB floor fills, fill {} block(s), floor {} sat/vB",
        config.data_min_bytes,
        config.data_max_bytes,
        config.small_txs_per_block,
        config.floor_pool_txs,
        config.fill_block_ratio,
        config.fee_rate_sat_vb,
    );
    run_block_loop(node1, move || {
        // Measure the mempool right after the new block drained it, and top it
        // back up to R blocks. At R < 1 blocks are deliberately partial.
        let mempool = meter
            .get_mempool_info()
            .map(|info| info.bytes as u64)
            .unwrap_or(0);
        let reserve = if config.fill_block_ratio >= 1.0 {
            BLOCK_VSIZE / 10
        } else {
            0
        };
        let target = (config.fill_block_ratio * BLOCK_VSIZE as f64) as u64 + reserve;
        let deficit = target.saturating_sub(mempool);
        let deficit2 = deficit / MINER_COUNT;
        let deficit3 = deficit - deficit2;
        let (result2, result3) = thread::scope(|scope| {
            // Floor fills first: the standing pool is the airtight guarantee,
            // and the data fill supplies the bulk behind it.
            let node2 = scope.spawn(|| {
                let fills = engine2.floor_round(pool2);
                let (txids, _) = engine2.hybrid_round(
                    deficit2,
                    small2,
                    effective_fanout,
                    config.enable_replaces,
                    config.replaces_per_miner,
                );
                fills + txids.len()
            });
            let node3 = scope.spawn(|| {
                let fills = engine3.floor_round(pool3);
                let (txids, _) = engine3.hybrid_round(
                    deficit3,
                    small3,
                    effective_fanout,
                    config.enable_replaces,
                    config.replaces_per_miner,
                );
                fills + txids.len()
            });
            (
                node2.join().expect("node2 spam thread panicked"),
                node3.join().expect("node3 spam thread panicked"),
            )
        });
        result2 + result3
    });
    Ok(())
}

fn run_raw_output_mode(node1: &Client, engine2: &mut RawSpammer, engine3: &mut RawSpammer) {
    let config = SpamConfig::global();
    tracing::info!(
        "Spam engine: raw transactions (USE_RAW_TX_SPAM=true), OUTPUT mode, {} sat/vB",
        config.fee_rate_sat_vb
    );
    if config.floor_pool_txs > 0 {
        tracing::info!(
            "NOTE: SPAM_FLOOR_POOL_TXS only applies to DATA/HYBRID mode (SPAM_TX_DATA_MAX_BYTES > 0); no floor fill pool in OUTPUT mode"
        );
    }
    // The raw engine always needs a branch pool (a single UTXO caps the whole
    // engine at one 25-tx unconfirmed chain), so 0 means one branch.
    let fanout_target = config.fanout_utxos.max(1);
    let (fixed2, fixed3) = config.fixed_shares();
    run_block_loop(node1, move || {
        let (txids2, txids3) = thread::scope(|scope| {
            let node2 = scope.spawn(|| {
                engine2.output_round(
                    fixed2,
                    fanout_target,
                    config.enable_replaces,
                    config.replaces_per_miner,
                )
            });
            let node3 = scope.spawn(|| {
                engine3.output_round(
                    fixed3,
                    fanout_target,
                    config.enable_replaces,
                    config.replaces_per_miner,
                )
            });
            (
                node2.join().expect("node2 spam thread panicked"),
                node3.join().expect("node3 spam thread panicked"),
            )
        });
        txids2.len() + txids3.len()
    });
}

fn run_node_wallets(node1: &Client, wallet2: Client, wallet3: Client) {
    let config = SpamConfig::global();
    tracing::info!("Spam engine: node wallets (USE_RAW_TX_SPAM=false)");
    // Sequential mode target: one shared burn address -- reusing a single
    // address is exactly what real dust spam looks like.
    let seq_addr = burn_address(0);
    // Batch mode address pool: a fixed set shared by both wallets. The keys
    // only need to be distinct within one transaction.
    let batch_addrs: Vec<Address> = (1..=config.sendmany_outputs).map(burn_address).collect();
    let (fixed2, fixed3) = config.fixed_shares();
    // Cover a block's spam, but never require more branches than we fan out to.
    let fanout_need = fixed2.min(config.fanout_utxos);

    run_block_loop(node1, move || {
        // One thread per wallet: fan-out top-up, this block's spam and each
        // wallet's RBF bumps run against their independent bitcoind instance.
        let (txids2, txids3) = thread::scope(|scope| {
            let node2 = scope.spawn(|| {
                node_wallet_spammer::spam_round(
                    &wallet2,
                    &config.wallet2_name,
                    "Node 2",
                    fixed2,
                    fanout_need,
                    config.fanout_utxos,
                    &seq_addr,
                    &batch_addrs,
                    config.enable_replaces,
                    config.replaces_per_miner,
                )
            });
            let node3 = scope.spawn(|| {
                node_wallet_spammer::spam_round(
                    &wallet3,
                    &config.wallet3_name,
                    "Node 3",
                    fixed3,
                    fanout_need,
                    config.fanout_utxos,
                    &seq_addr,
                    &batch_addrs,
                    config.enable_replaces,
                    config.replaces_per_miner,
                )
            });
            (
                node2.join().expect("node2 spam thread panicked"),
                node3.join().expect("node3 spam thread panicked"),
            )
        });
        txids2.len() + txids3.len()
    });
}
