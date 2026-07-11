//! Reorganization mechanics: invalidate a competing branch and mine a longer
//! replacement branch, then make sure the network adopts it.

use crate::{
    chain::{
        branch_to_orphan, last_blocks, live_mempool_weighted, mine_exact, pack_by_weight,
        print_blocks, BlockTx, BLOCK_WEIGHT_BUDGET,
    },
    config::ReorgConfig,
    double_spend::{build_plan, DoubleSpendPlan},
    wallet::inject_transactions,
};
use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use std::{
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The mining controller may extend the old chain on the other miner while
/// the replacements are being mined; if it lands a block, depth+1 new blocks
/// only tie and the network never reorgs. Poll a witness node until it adopts
/// the reorg node's tip, mining one extra block per round to outpace the old
/// chain. Gives up (with a warning) after `max_extra` extra blocks. In empty
/// mode the extra blocks are empty too, so a chaos reorg does not quietly
/// confirm the orphaned txs through its race-winning block.
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
                        tracing::info!(
                            "Network adopted the new chain after {extra} extra block(s)"
                        );
                    }
                    return Ok(());
                }
                Ok(_) => thread::sleep(Duration::from_millis(250)),
                Err(error) => {
                    tracing::warn!("Witness node '{witness_name}' unreachable ({error}), cannot verify the network reorged");
                    return Ok(());
                }
            }
        }
        if extra == max_extra {
            break;
        }
        tracing::info!("'{witness_name}' is still on the old chain (miners kept extending it), mining 1 extra block...");
        if empty_mode {
            mine_exact(node, mine_address, &[])?;
        } else {
            node.generate_to_address(1, mine_address)?;
        }
    }
    tracing::warn!("the network did not adopt the new chain after {max_extra} extra blocks");
    Ok(())
}

/// Force a chain reorganization by invalidating the last configured number of
/// blocks and mining one additional replacement block, making the new branch
/// strictly longer than the one it replaces.
pub fn run(node: &Client, witness: Option<(&Client, &str)>) -> Result<(), bitcoincore_rpc::Error> {
    let config = ReorgConfig::global();
    let tip = node.get_block_count()?;
    if tip < config.depth + 1 {
        tracing::warn!(
            "Chain too short (height {tip}) to reorg {} blocks, skipping",
            config.depth
        );
        return Ok(());
    }

    tracing::info!(
        "\n=== Simulating a reorg of the last {} blocks ===\n",
        config.depth
    );
    tracing::info!("--- Last {} blocks BEFORE reorg ---", config.depth + 2);
    let before = last_blocks(node, config.depth + 2)?;
    print_blocks(&before);

    // Capture the exact branch about to be orphaned (full txid lists) before
    // invalidation, so the double-spend planner can walk the rolled-back txs.
    // Only needed for a non-empty reorg with the feature enabled.
    let plan_branch = if !config.empty_mode && config.double_spend_pct > 0 {
        Some(branch_to_orphan(node, config.depth)?)
    } else {
        None
    };

    let target_height = tip - config.depth + 1;
    let target_hash = node.get_block_hash(target_height)?;
    let target_time = node.get_block_info(&target_hash)?.time as u64;

    tracing::info!("\nInvalidating block {target_height} ({target_hash})...");
    node.invalidate_block(&target_hash)?;

    // Count the txs the orphaned blocks returned to the mempool (for the
    // summary). The mining below reads the mempool live for each block, so RBF
    // replacements that arrive mid-reorg are handled without special-casing.
    let returned = node.get_raw_mempool()?.len();
    tracing::info!("{returned} transactions returned to the mempool from the orphaned blocks");

    // A replacement block with the same timestamp and coinbase as the
    // invalidated one hashes identically and is rejected as known-invalid,
    // so wait until the clock has moved past the original block's time.
    while now_secs() <= target_time {
        thread::sleep(Duration::from_millis(250));
    }

    let blocks_to_mine = config.depth + 1;
    let plan: Option<DoubleSpendPlan> = if config.empty_mode {
        // Chaos reorg: mine empty replacement blocks and leave the orphaned
        // txs unconfirmed in the mempool. adds_new_txs is ignored -- empty
        // means empty. Double-spend is ignored too: empty means empty.
        if config.double_spend_pct > 0 {
            tracing::info!(
                "Double-spend mode ignored in empty (chaos) reorg (REORG_DOUBLE_SPEND_PCT={})",
                config.double_spend_pct
            );
        }
        tracing::info!("Mining {blocks_to_mine} EMPTY replacement blocks (chaos reorg, one extra so the new chain wins network-wide)...");
        for _ in 0..blocks_to_mine {
            mine_exact(node, &config.mine_address, &[])?;
        }
        None
    } else {
        // Build the permanent-drop plan from the rolled-back state (roots are
        // on-chain UTXOs again, orphaned txs are back in the mempool). A pct of
        // 0 or an empty branch yields an empty plan and today's behavior.
        let plan = build_plan(
            node,
            plan_branch.as_deref().unwrap_or(&[]),
            config.double_spend_pct,
        );
        if config.double_spend_pct > 0 {
            plan.log_selection();
        }

        // Seed the mempool with brand-new txs this node "saw first" so the
        // winning chain carries them alongside the returned txs.
        if config.adds_new_txs > 0 {
            inject_transactions(node, config.adds_new_txs);
        }

        // Re-mine the returned mempool plus the double-spend conflicts across
        // the replacement blocks. The conflicts are raw hex that must land, so
        // they are reserved first (spread across the blocks). The rest of each
        // block's weight budget is packed from the mempool, read fresh each
        // round -- so txs mined into earlier blocks drop out, RBF replacements
        // are reflected, and the dropped originals + descendants stay filtered.
        //
        // Packing by *weight* (not by tx count) is essential: spam txs are fat,
        // so only ~one block's worth fits per block. Splitting the whole backlog
        // by block count instead would hand a single generateblock more weight
        // than the 4M consensus limit, which Core rejects (`bad-blk-length`) and
        // the recovery ladder then turns into an empty block -- the reorg would
        // silently degrade to empty mode. Whatever exceeds the replacement
        // blocks' total capacity stays in the mempool for later real blocks.
        let mut raw = plan.raw_conflicts();
        if raw.is_empty() {
            tracing::info!("Mining {blocks_to_mine} replacement blocks from the live mempool (one extra so the new chain wins network-wide)...");
        } else {
            tracing::info!(
                "Mining {blocks_to_mine} replacement blocks from the live mempool plus {} double-spend conflict(s) (one extra so the new chain wins network-wide)...",
                raw.len()
            );
        }
        for index in 0..blocks_to_mine as usize {
            let blocks_left = blocks_to_mine as usize - index;

            // Reserve this block's share of the raw conflicts and subtract their
            // weight from the budget before packing mempool txs.
            let raw_take = raw.len().div_ceil(blocks_left).min(raw.len());
            let raw_items: Vec<(String, u64)> = raw.drain(..raw_take).collect();
            let raw_weight: u64 = raw_items.iter().map(|(_, w)| *w).sum();
            let budget = BLOCK_WEIGHT_BUDGET.saturating_sub(raw_weight);

            let mempool = live_mempool_weighted(node, &plan.excluded_mempool_txids)?;
            let packed = pack_by_weight(&mempool, budget);

            let mut items: Vec<BlockTx> = Vec::with_capacity(raw_items.len() + packed.len());
            items.extend(raw_items.into_iter().map(|(hex, _)| BlockTx::RawHex(hex)));
            items.extend(packed.into_iter().map(BlockTx::Mempool));
            mine_exact(node, &config.mine_address, &items)?;
        }
        Some(plan)
    };

    // Make sure the rest of the network actually switched to the new chain
    // before declaring success, then let it propagate before reporting.
    if let Some((witness, witness_name)) = witness {
        ensure_network_adopts(
            node,
            witness,
            witness_name,
            &config.mine_address,
            10,
            config.empty_mode,
        )?;
    }
    thread::sleep(Duration::from_secs(2));

    tracing::info!("\n--- Last {} blocks AFTER reorg ---", config.depth + 3);
    let after = last_blocks(node, config.depth + 3)?;
    print_blocks(&after);

    tracing::info!("\n--- Replaced blocks ---");
    for (height, old_hash, old_txs) in before.iter().rev() {
        if let Some((_, new_hash, new_txs)) = after.iter().find(|(h, _, _)| h == height) {
            if new_hash != old_hash {
                tracing::info!(
                    "{height} : {old_hash} ({old_txs} txs) => {new_hash} ({new_txs} txs)"
                );
            }
        }
    }

    if let Some(plan) = &plan {
        plan.log_dropped(node);
    }

    tracing::info!(
        "\n=== Reorg done: blocks from height {target_height} replaced, new tip {} ===",
        node.get_block_count()?
    );
    Ok(())
}
