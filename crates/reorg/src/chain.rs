//! Chain and mempool helpers on a single node: readiness polling, recent-block
//! inspection, topological mempool reads and exact-content block mining.

use bitcoincore_rpc::{
    bitcoin::{Address, BlockHash, Txid},
    Client, RpcApi,
};
use serde_json::json;
use std::collections::HashSet;

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

/// Txids currently in the mempool, ordered parents-first (ascending ancestor
/// count). A leading slice of this list is always a valid set to mine into one
/// block: a child never precedes its parent, and every parent still in the
/// mempool sorts ahead of it.
pub fn live_mempool_topo(node: &Client) -> Result<Vec<Txid>, bitcoincore_rpc::Error> {
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

/// Like [`live_mempool_topo`] but with `excluded` txids removed. Used by the
/// double-spend path to keep the deliberately-dropped originals and their
/// descendants out of the replacement blocks. The parents-first invariant
/// survives the filter: a kept child's parent can only be excluded if the child
/// itself were an excluded descendant, so no leading slice ever splits a parent
/// from a child.
pub fn live_mempool_topo_filtered(
    node: &Client,
    excluded: &HashSet<Txid>,
) -> Result<Vec<Txid>, bitcoincore_rpc::Error> {
    Ok(live_mempool_topo(node)?
        .into_iter()
        .filter(|txid| !excluded.contains(txid))
        .collect())
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
    let mempool_left = filtered.len() - raw.len();
    tracing::info!("  mining an empty block, {mempool_left} tx(s) left for the next block");
    generate_block(node, mine_address, &Vec::<String>::new())
}
