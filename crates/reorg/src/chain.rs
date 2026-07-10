//! Chain and mempool helpers on a single node: readiness polling, recent-block
//! inspection, topological mempool reads and exact-content block mining.

use bitcoincore_rpc::{
    bitcoin::{Address, Txid},
    Client, RpcApi,
};
use serde_json::json;
use std::collections::HashSet;

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

/// Mine exactly `txids` (plus the coinbase) into one block with `generateblock`,
/// which -- unlike `generate_to_address` -- never pulls the rest of the mempool
/// in, so mining one block can never strand a later block's transactions.
/// If a tx went invalid since it was selected (e.g. RBF-replaced mid-reorg),
/// re-filter to what is still in the mempool and retry once; if nothing valid
/// remains (or the rejection was not about a missing tx), mine a real empty
/// block. Never drains the mempool.
pub fn mine_exact(
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
        Err(e) => tracing::warn!(
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
        tracing::info!(
            "  dropped {dropped} stale tx(s), mining the remaining {}",
            filtered.len()
        );
        node.call::<serde_json::Value>(
            "generateblock",
            &[json!(mine_address.to_string()), json!(filtered)],
        )?;
    } else {
        tracing::info!(
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
