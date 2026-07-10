//! Continuous mining loop: alternating or weighted miner selection, fixed or
//! poisson block intervals, with reorg and external-block reporting.

use crate::chain_view::{sync_view, ChainView, REORG_WINDOW};
use crate::config::MiningConfig;
use crate::rng::Rng;
use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use simchain_common::{rpc_retry, wait_for_height};
use std::{thread, time::Duration};

// Continuous mining loop. The controller remembers the recent chain --
// heights, hashes, and which blocks it mined itself -- so a reorg (the
// reorg simulator rewriting recent blocks) is reported with its full
// extent: fork point, replaced range and new tip, with the replacement
// blocks flagged EXTERNAL because someone else mined them. Like a real
// miner the controller keeps mining on whatever tip the node reports --
// generate_to_address already does that -- so detection only makes the
// events visible here; nothing needs to be controlled.
pub fn run(
    seed: u64,
    mut rng: Rng,
    node2: &Client,
    node3: &Client,
    addr2: &Address,
    addr3: &Address,
) -> ! {
    let config = MiningConfig::global();
    let mean_secs = config.mean_secs;
    let poisson = config.interval_mode.is_poisson();
    let interval_bounds = config.interval_bounds;
    let miner_weights = config.miner_weights;
    let stochastic = poisson || miner_weights.is_some();

    let mut view = ChainView::new();
    let mut last = rpc_retry("get initial mining-loop block count", || {
        node2.get_block_count()
    });
    // Seed the view with the recent chain so even the first reorg gets an
    // accurate fork point. Bootstrap blocks are seeded as not-ours, which is
    // harmless: seeded heights are never re-walked unless a reorg replaces
    // them, and replacement blocks are external by definition.
    for h in last.saturating_sub(REORG_WINDOW)..=last {
        if let Ok(hash) = node2.get_block_hash(h) {
            view.record(h, hash, false);
        }
    }

    let bounds_description = interval_bounds.description();
    let interval_description = if poisson {
        format!("poisson mean={mean_secs}s, bounds={}", bounds_description)
    } else {
        format!("fixed {mean_secs}s")
    };
    let weights_description = match miner_weights {
        Some(weights) => format!("{},{} (node2,node3)", weights.node2, weights.node3),
        None => "alternate".to_string(),
    };
    if stochastic {
        tracing::info!(
            "Mining config: interval={interval_description}, weights={weights_description}, rng_seed={seed}"
        );
    } else {
        tracing::info!(
            "Mining config: interval={interval_description}, weights={weights_description}"
        );
    }

    let mut toggle = true;
    loop {
        let start_time = std::time::Instant::now();

        let target = if poisson {
            let sampled = rng.next_exp(mean_secs as f64);
            let target_secs = interval_bounds.apply(sampled);
            tracing::info!(
                "TIMING sampled interval {sampled:.2}s, target {target_secs:.2}s (poisson, mean {mean_secs}s, bounds={})",
                bounds_description
            );
            Duration::from_secs_f64(target_secs)
        } else {
            Duration::from_secs(mean_secs)
        };

        let pick_node2 = match miner_weights {
            Some(weights) => rng.next_below(weights.total) < weights.node2,
            None => toggle,
        };
        let (miner, other, addr, name) = if pick_node2 {
            (node2, node3, addr2, "Node 2")
        } else {
            (node3, node2, addr3, "Node 3")
        };

        // Catch up with the node before mining: report any reorg and any
        // externally mined blocks that appeared since the last round.
        last = sync_view(&mut view, miner, last);

        let mined = match miner.generate_to_address(1, addr) {
            Ok(mined) => mined,
            Err(error) => {
                tracing::warn!("{name} => Block generation failed ({error}), retrying next round");
                // Do not re-issue a timed-out generate: it may have mined a
                // block. The next sync_view re-derives live chain state. Such
                // a block is reported as EXTERNAL because its returned hash
                // was never available for attribution to this controller.
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        // Identify the new block by the hash generate returned instead of
        // the tip counter, which races with blocks arriving from elsewhere.
        let hash = mined[0];
        let mined_height = rpc_retry("get newly mined block header", || {
            miner.get_block_header_info(&hash)
        })
        .height as u64;
        tracing::info!("{name} => Mined 1 block [{mined_height}] {hash} to address {addr}");
        view.record(mined_height, hash, true);
        last = last.max(mined_height);
        wait_for_height(other, mined_height, Duration::from_millis(100));

        toggle = !toggle;

        let elapsed = start_time.elapsed();
        if elapsed < target {
            thread::sleep(target - elapsed);
        }
    }
}
