//! The controller's view of the recent chain, used to detect and report
//! reorgs and externally mined blocks.

use bitcoincore_rpc::{bitcoin::BlockHash, Client, RpcApi};
use std::collections::{BTreeMap, HashSet};

// How many recent blocks to remember for reorg analysis. Reorgs deeper than
// this window are still detected, but the fork point is then reported as the
// bottom of the window (the same rule chainwatch.sh uses).
pub const REORG_WINDOW: u64 = 100;

// The controller's view of the recent chain: the hash it last observed at
// each height, plus the set of hashes it mined itself. Comparing the node's
// chain against `seen` exposes reorgs (and their fork point), and any block
// missing from `own` was mined by someone else -- the reorg simulator, a
// manual generate call, etc.
pub struct ChainView {
    seen: BTreeMap<u64, BlockHash>,
    own: HashSet<BlockHash>,
}

impl ChainView {
    pub fn new() -> Self {
        ChainView {
            seen: BTreeMap::new(),
            own: HashSet::new(),
        }
    }

    pub fn record(&mut self, height: u64, hash: BlockHash, mined_by_us: bool) {
        self.seen.insert(height, hash);
        if mined_by_us {
            self.own.insert(hash);
        }
        // Keep both collections bounded to the reorg window.
        let floor = height.saturating_sub(REORG_WINDOW);
        while let Some((&h, _)) = self.seen.first_key_value() {
            if h >= floor {
                break;
            }
            if let Some(old) = self.seen.remove(&h) {
                self.own.remove(&old);
            }
        }
    }

    // Walk down from `below` to the highest recorded height whose hash still
    // matches the node's chain: the fork point (last common block).
    fn find_fork(&self, node: &Client, below: u64) -> u64 {
        for (&h, hash) in self.seen.range(..=below).rev() {
            if node.get_block_hash(h).ok().as_ref() == Some(hash) {
                return h;
            }
        }
        // Nothing matches: the reorg is deeper than the window.
        self.seen
            .first_key_value()
            .map_or(0, |(&h, _)| h.saturating_sub(1))
    }
}

// Reconcile the node's chain with the controller's recorded view and return
// the node's current tip. A rewritten block triggers a REORG report with the
// fork point, the replaced range and the new tip; every walked block the
// controller did not mine itself is flagged EXTERNAL.
pub fn sync_view(view: &mut ChainView, node: &Client, last: u64) -> u64 {
    let tip = match node.get_block_count() {
        Ok(t) => t,
        Err(_) => return last,
    };
    let base = last.min(tip);

    // If the hash recorded at min(last, tip) still matches, the chain only
    // grew; otherwise history at or below that height was rewritten.
    let reorged = match view.seen.get(&base) {
        Some(hash) => node.get_block_hash(base).ok().as_ref() != Some(hash),
        None => false,
    };

    let from = if reorged {
        let fork = view.find_fork(node, base);
        tracing::info!(
            "REORG detected: blocks [{}..{}] replaced; forked at [{}], new tip [{}], mining continues on the new chain",
            fork + 1, last, fork, tip
        );
        // Forget the replaced blocks (and drop them from `own`: a replaced
        // block of ours is no longer on the chain). The walk below records
        // their replacements.
        let stale = view.seen.split_off(&(fork + 1));
        for hash in stale.values() {
            view.own.remove(hash);
        }
        fork + 1
    } else {
        last + 1
    };

    for h in from..=tip {
        // A block can vanish mid-walk if another reorg lands right now; stop
        // and let the next round re-sync.
        let Ok(hash) = node.get_block_hash(h) else {
            break;
        };
        let mined_by_us = view.own.contains(&hash);
        if !mined_by_us {
            tracing::info!("EXTERNAL block [{h}] {hash} (not mined by this controller)");
        }
        view.record(h, hash, mined_by_us);
    }
    tip
}
