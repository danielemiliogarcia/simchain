//! Continuous mining loop: alternating or weighted miner selection, fixed or
//! poisson block intervals, with reorg and external-block reporting.

use crate::chain_view::{sync_view, ChainView, REORG_WINDOW};
use crate::control::{now_ms, IntervalWait, MiningControl};
use crate::rng::Rng;
use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use simchain_common::internal_api::LastMinedBlock;
use simchain_common::live_tuning::MiningTuning;
use simchain_common::{rpc_retry, wait_for_height};
use std::sync::Arc;
use std::time::Duration;

// Continuous mining loop. The controller remembers the recent chain --
// heights, hashes, and which blocks it mined itself -- so a reorg (the
// reorg simulator rewriting recent blocks) is reported with its full
// extent: fork point, replaced range and new tip, with the replacement
// blocks flagged EXTERNAL because someone else mined them. Like a real
// miner the controller keeps mining on whatever tip the node reports --
// generate_to_address already does that -- so detection only makes the
// events visible here; nothing needs to be controlled.
pub fn run(
    control: Arc<MiningControl>,
    mut rng: Rng,
    node2: &Client,
    node3: &Client,
    addr2: &Address,
    addr3: &Address,
) -> ! {
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

    let mut toggle = true;
    // Force the scheduler-local RNG/toggle state to be initialized from the
    // effective policy even when a control-plane reconciliation landed
    // during bootstrap.
    let mut observed_generation = u64::MAX;
    let mut logged_generation = u64::MAX;
    loop {
        let policy = control.mining_safe_point(&mut rng, &mut toggle, &mut observed_generation);
        let generation = control.current_generation();
        if logged_generation != generation {
            log_policy(&policy, generation, control.status().effective_rng_seed);
            logged_generation = generation;
        }

        // Preserve the original cadence semantics: generate immediately,
        // then wait only the unused portion of this sampled interval. RPC and
        // propagation time therefore count toward block spacing.
        let interval_started = std::time::Instant::now();
        let target = next_interval(&policy, &mut rng);

        // Re-enter after sampling. A policy update here resets the RNG and
        // must resample rather than use an interval from the old generation.
        let policy = control.mining_safe_point(&mut rng, &mut toggle, &mut observed_generation);
        if control.current_generation() != generation {
            continue;
        }
        let pick_node2 = match policy.miner_weights {
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

        if !control.begin_generate(generation) {
            continue;
        }
        let mined = match miner.generate_to_address(1, addr) {
            Ok(mined) => mined,
            Err(error) => {
                let message = format!("{name} block generation failed: {error}");
                tracing::warn!("{message}; retrying next round");
                control.record_error(message);
                control.finish_generate(None, Some(last));
                // Do not re-issue a timed-out generate: it may have mined a
                // block. The next sync_view re-derives live chain state. Such
                // a block is reported as EXTERNAL because its returned hash
                // was never available for attribution to this controller.
                let _ = control.wait_interval(Duration::from_secs(2), generation);
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
        control.finish_generate(
            Some(LastMinedBlock {
                height: mined_height,
                hash: hash.to_string(),
                miner: name.to_string(),
                mined_at_ms: now_ms(),
            }),
            Some(mined_height),
        );

        toggle = !toggle;
        let elapsed = interval_started.elapsed();
        if elapsed < target
            && control.wait_interval(target - elapsed, generation) == IntervalWait::Interrupted
        {
            continue;
        }
    }
}

fn next_interval(policy: &MiningTuning, rng: &mut Rng) -> Duration {
    if policy.interval_mode.is_poisson() {
        let sampled = rng.next_exp(policy.mean_secs as f64);
        let target_secs = policy.interval_bounds.apply(sampled);
        tracing::info!(
            "TIMING sampled interval {sampled:.2}s, target {target_secs:.2}s (poisson, mean {}s, bounds={})",
            policy.mean_secs,
            policy.interval_bounds.description()
        );
        Duration::from_secs_f64(target_secs)
    } else {
        Duration::from_secs(policy.mean_secs)
    }
}

fn log_policy(policy: &MiningTuning, generation: u64, seed: u64) {
    let interval = if policy.interval_mode.is_poisson() {
        format!(
            "poisson mean={}s, bounds={}",
            policy.mean_secs,
            policy.interval_bounds.description()
        )
    } else {
        format!("fixed {}s", policy.mean_secs)
    };
    let weights = policy.miner_weights.map_or_else(
        || "alternate".to_string(),
        |weights| format!("{},{} (node2,node3)", weights.node2, weights.node3),
    );
    tracing::info!(
        generation,
        seed,
        "Mining config: interval={interval}, weights={weights}"
    );
}
