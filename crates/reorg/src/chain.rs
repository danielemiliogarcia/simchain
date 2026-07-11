//! Chain and mempool helpers on a single node: readiness polling, recent-block
//! inspection, topological mempool reads and exact-content block mining.

use bitcoincore_rpc::{
    bitcoin::{Address, BlockHash, Txid},
    Client, RpcApi,
};
use serde_json::json;
use std::collections::HashSet;

/// Consensus cap on block weight. A block whose transactions push past this is
/// rejected with `bad-blk-length`, so replacement blocks are packed below it.
const MAX_BLOCK_WEIGHT: u64 = 4_000_000;

/// Weight budget for one replacement block's explicit transaction list. Kept
/// below [`MAX_BLOCK_WEIGHT`] with headroom for the coinbase `generateblock`
/// appends and for the vsize*4 upper-bound estimate a pre-0.19 node would use,
/// so a packed list never trips the consensus size check.
pub const BLOCK_WEIGHT_BUDGET: u64 = MAX_BLOCK_WEIGHT - 100_000;

/// One block on the branch a reorg is about to orphan, with its full ordered
/// txid list (coinbase first). Captured before invalidation so the double-spend
/// planner can walk the exact txs being rolled back.
#[derive(Clone, Debug)]
pub struct BranchBlock {
    pub height: u64,
    pub hash: BlockHash,
    pub txids: Vec<Txid>,
}

/// The exact slice of the chain a `depth`-block reorg will orphan: heights
/// `tip-depth+1 ..= tip`, returned oldest-first so downstream selection is
/// deterministic. Callers must have already checked the chain is long enough
/// (`tip >= depth`).
pub fn branch_to_orphan(
    node: &Client,
    depth: u64,
) -> Result<Vec<BranchBlock>, bitcoincore_rpc::Error> {
    let tip = node.get_block_count()?;
    let first = tip.saturating_sub(depth) + 1;
    let mut blocks = Vec::new();
    for height in first..=tip {
        let hash = node.get_block_hash(height)?;
        let info = node.get_block_info(&hash)?;
        blocks.push(BranchBlock {
            height,
            hash,
            txids: info.tx,
        });
    }
    Ok(blocks)
}

/// An item to mine into a `generateblock` call: either a txid already in the
/// mempool, or a raw transaction (hex) that is not -- Bitcoin Core accepts a
/// mixed ordered list of both. The double-spend conflicts are `RawHex` because
/// they must land in the block regardless of mempool policy.
#[derive(Clone, Debug)]
pub enum BlockTx {
    Mempool(Txid),
    RawHex(String),
}

impl BlockTx {
    fn to_arg(&self) -> String {
        match self {
            BlockTx::Mempool(txid) => txid.to_string(),
            BlockTx::RawHex(hex) => hex.clone(),
        }
    }
}

/// (height, hash, tx count) for the last `count` blocks, tip first.
pub fn last_blocks(
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
pub fn print_blocks(blocks: &[(u64, String, usize)]) {
    for (height, hash, txs) in blocks.iter().rev() {
        tracing::info!("{height} : {txs:>3} txs -> {hash}");
    }
}

/// One mempool transaction with the block weight it contributes. Ordered
/// parents-first by the list it lives in.
#[derive(Clone, Copy, Debug)]
pub struct MempoolTx {
    pub txid: Txid,
    pub weight: u64,
}

/// Mempool transactions with their weights, ordered parents-first (ascending
/// ancestor count, txid as a deterministic tiebreak) and with `excluded` txids
/// removed. A leading slice is always a valid set to mine into one block: a
/// child never precedes its parent, and every parent still in the mempool sorts
/// ahead of it. The filter preserves that -- a kept child's parent could only
/// be excluded if the child were itself an excluded descendant.
///
/// Reads the whole mempool in one verbose RPC (weights included) rather than a
/// `getmempoolentry` per tx, so a multi-thousand-tx backlog costs one round-trip.
pub fn live_mempool_weighted(
    node: &Client,
    excluded: &HashSet<Txid>,
) -> Result<Vec<MempoolTx>, bitcoincore_rpc::Error> {
    let mut entries: Vec<(u64, Txid, u64)> = node
        .get_raw_mempool_verbose()?
        .into_iter()
        .filter(|(txid, _)| !excluded.contains(txid))
        .map(|(txid, entry)| {
            // `weight` is present on Core >= 0.19; fall back to vsize*4 (an
            // upper bound, safe for budgeting) if an older node omits it.
            let weight = entry.weight.unwrap_or(entry.vsize * 4);
            (entry.ancestor_count, txid, weight)
        })
        .collect();
    // Parents-first; txid tiebreak makes ties deterministic (the verbose
    // mempool arrives as an unordered map).
    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(entries
        .into_iter()
        .map(|(_, txid, weight)| MempoolTx { txid, weight })
        .collect())
}

/// Greedily take a parents-first prefix of `mempool` whose combined weight fits
/// in `budget`. Stops at the first tx that would overflow rather than skipping
/// it, so the result stays a valid topological prefix and no parent is split
/// from a child. A standard mempool tx is far smaller than a block budget, so
/// this never strands a tx that could otherwise be mined. Pure and deterministic.
pub fn pack_by_weight(mempool: &[MempoolTx], budget: u64) -> Vec<Txid> {
    let mut chosen = Vec::new();
    let mut used: u64 = 0;
    for tx in mempool {
        if used + tx.weight > budget {
            break;
        }
        used += tx.weight;
        chosen.push(tx.txid);
    }
    chosen
}

/// Issue one `generateblock` with an explicit item list. Returns `Ok(())` on
/// success; the caller decides how to react to an error.
fn generate_block(
    node: &Client,
    mine_address: &Address,
    list: &[String],
) -> Result<(), bitcoincore_rpc::Error> {
    node.call::<serde_json::Value>(
        "generateblock",
        &[json!(mine_address.to_string()), json!(list)],
    )
    .map(|_| ())
}

/// Mine exactly `items` (plus the coinbase) into one block with `generateblock`,
/// which -- unlike `generate_to_address` -- never pulls the rest of the mempool
/// in, so mining one block can never strand a later block's transactions.
///
/// [`BlockTx::RawHex`] items are the deliberate double-spend conflicts and must
/// land; [`BlockTx::Mempool`] items may go stale mid-reorg (e.g. RBF-replaced).
/// On rejection the recovery ladder is: (1) re-filter mempool txids to what is
/// still present while keeping every raw conflict, and retry; (2) if that still
/// fails, mine just the raw conflicts, since they are the point of the reorg;
/// (3) fall back to a real empty block. Never drains the mempool.
pub fn mine_exact(
    node: &Client,
    mine_address: &Address,
    items: &[BlockTx],
) -> Result<(), bitcoincore_rpc::Error> {
    let full: Vec<String> = items.iter().map(BlockTx::to_arg).collect();
    match generate_block(node, mine_address, &full) {
        Ok(()) => return Ok(()),
        Err(e) => tracing::warn!(
            "generateblock rejected {} item(s) ({e}), re-filtering to the live mempool...",
            full.len()
        ),
    }

    let live: HashSet<Txid> = node.get_raw_mempool()?.into_iter().collect();
    let raw: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            BlockTx::RawHex(hex) => Some(hex.clone()),
            BlockTx::Mempool(_) => None,
        })
        .collect();
    let filtered: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            BlockTx::RawHex(hex) => Some(hex.clone()),
            BlockTx::Mempool(txid) => live.contains(txid).then(|| txid.to_string()),
        })
        .collect();

    // Some mempool txids went stale but salvageable content remains: retry with
    // the raw conflicts plus whatever mempool txids are still present.
    let dropped = full.len() - filtered.len();
    if dropped > 0 && !filtered.is_empty() {
        tracing::info!(
            "  dropped {dropped} stale tx(s), mining the remaining {}",
            filtered.len()
        );
        match generate_block(node, mine_address, &filtered) {
            Ok(()) => return Ok(()),
            Err(e) => tracing::warn!("  filtered retry still rejected ({e})"),
        }
    }

    // The raw conflicts are the whole point of the reorg, so try them alone
    // before giving up (the first attempt may have failed on a stale mempool
    // txid rather than the conflicts).
    if !raw.is_empty() && raw.len() < filtered.len() {
        tracing::info!("  mining the {} raw replacement(s) only", raw.len());
        match generate_block(node, mine_address, &raw) {
            Ok(()) => return Ok(()),
            Err(e) => tracing::warn!("  raw-only block rejected ({e})"),
        }
    }

    // All evicted, or the rejection was not about a missing tx: mine an empty
    // block; the untouched txs stay in the mempool for the next block's sweep.
    // If this block carried raw conflicts, they just failed to land -- warn
    // loudly rather than logging "0 tx(s) left" as if there were nothing to do;
    // the corresponding permanent-drop did not happen this block (log_dropped
    // confirms on-chain and reports any miss).
    let mempool_left = filtered.len() - raw.len();
    if raw.is_empty() {
        tracing::info!("  mining an empty block, {mempool_left} tx(s) left for the next block");
    } else {
        tracing::warn!(
            "  mining an empty block: {} raw replacement(s) FAILED to mine, \
             {mempool_left} mempool tx(s) left for the next block",
            raw.len()
        );
    }
    generate_block(node, mine_address, &Vec::<String>::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoincore_rpc::bitcoin::hashes::Hash;

    fn mtx(n: u8, weight: u64) -> MempoolTx {
        MempoolTx {
            txid: Txid::from_byte_array([n; 32]),
            weight,
        }
    }

    #[test]
    fn pack_by_weight_takes_prefix_that_fits() {
        let mempool = vec![mtx(1, 400), mtx(2, 400), mtx(3, 400)];
        // Budget fits the first two but not the third.
        let chosen = pack_by_weight(&mempool, 900);
        assert_eq!(chosen, vec![mempool[0].txid, mempool[1].txid]);
    }

    #[test]
    fn pack_by_weight_stops_at_first_overflow_keeping_topo_prefix() {
        // A big tx in the middle stops packing there rather than skipping it to
        // grab the smaller tx behind it, which would strand a parent.
        let mempool = vec![mtx(1, 100), mtx(2, 5000), mtx(3, 100)];
        let chosen = pack_by_weight(&mempool, 1000);
        assert_eq!(chosen, vec![mempool[0].txid]);
    }

    #[test]
    fn pack_by_weight_empty_when_first_tx_exceeds_budget() {
        assert!(pack_by_weight(&[mtx(1, 5000)], 1000).is_empty());
    }

    #[test]
    fn pack_by_weight_takes_all_when_budget_is_ample() {
        let mempool = vec![mtx(1, 400), mtx(2, 400)];
        let chosen = pack_by_weight(&mempool, BLOCK_WEIGHT_BUDGET);
        assert_eq!(chosen.len(), 2);
    }
}
