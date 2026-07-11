//! Permanent-drop (double-spend) planning for a reorg.
//!
//! When `REORG_DOUBLE_SPEND_PCT > 0`, a fraction of the orphaned wallet
//! transactions are replaced on the winning chain by a same-input,
//! different-output conflict, so the originals become permanently invalid and
//! can never re-confirm. This module owns eligibility detection, conflict
//! construction, descendant exclusion, and the operator-facing logging; the
//! reorg loop only asks for a [`DoubleSpendPlan`] and mines what it returns.
//!
//! The plan must be built *after* invalidation: only then are the orphaned txs
//! back in the mempool (so descendant closure is queryable) and their root
//! inputs visible again as on-chain UTXOs (so a same-input conflict can be
//! signed and `gettxout(..., include_mempool=false)` distinguishes roots from
//! descendants of another orphaned tx).

use crate::chain::BranchBlock;
use crate::wallet::resolve_wallet;
use bitcoincore_rpc::{
    bitcoin::{Amount, Txid},
    json::CreateRawTransactionInput,
    Client, RpcApi,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};

/// Below this the replacement's single output would be an unspendable dust
/// output that Bitcoin Core rejects; such a tx is skipped as ineligible.
const DUST_SAT: u64 = 294;

/// A single permanent-drop: the orphaned original, the conflicting replacement
/// mined in its place, and the mempool descendants that die with it.
#[derive(Clone, Debug)]
pub struct ReplacementTx {
    pub original_txid: Txid,
    pub replacement_txid: Txid,
    pub raw_hex: String,
    pub pruned_descendants: Vec<Txid>,
}

/// The outcome of planning: the conflicts to mine, the txids that must be kept
/// out of the replacement blocks (selected originals plus their descendants),
/// and enough context to log why nothing was selected.
#[derive(Clone, Debug)]
pub struct DoubleSpendPlan {
    pub replacements: Vec<ReplacementTx>,
    pub excluded_mempool_txids: HashSet<Txid>,
    pub configured_pct: u8,
    pub eligible_count: usize,
    wallet_txs_seen: usize,
    wallet_resolved: bool,
}

impl DoubleSpendPlan {
    fn empty(
        pct: u8,
        wallet_resolved: bool,
        eligible_count: usize,
        wallet_txs_seen: usize,
    ) -> Self {
        Self {
            replacements: Vec::new(),
            excluded_mempool_txids: HashSet::new(),
            configured_pct: pct,
            eligible_count,
            wallet_txs_seen,
            wallet_resolved,
        }
    }

    /// Raw hex of every conflict, in selection order, for the mining loop.
    pub fn raw_conflicts(&self) -> Vec<String> {
        self.replacements
            .iter()
            .map(|r| r.raw_hex.clone())
            .collect()
    }

    /// Log the up-front summary: configured pct, eligible/selected counts, and
    /// (when nothing was selected) the most likely reason.
    pub fn log_selection(&self) {
        if self.replacements.is_empty() {
            tracing::info!(
                "Double-spend mode: 0 of {} eligible wallet txs selected (REORG_DOUBLE_SPEND_PCT={}); {}",
                self.eligible_count,
                self.configured_pct,
                self.zero_reason()
            );
            return;
        }
        tracing::info!(
            "Double-spend mode: selected {} of {} eligible wallet txs (REORG_DOUBLE_SPEND_PCT={})",
            self.replacements.len(),
            self.eligible_count,
            self.configured_pct
        );
        for r in &self.replacements {
            tracing::info!(
                "  {} -> {} ({} descendants pruned)",
                r.original_txid,
                r.replacement_txid,
                r.pruned_descendants.len()
            );
        }
    }

    /// Log the dedicated post-reorg section proving each original was dropped.
    pub fn log_dropped(&self) {
        if self.replacements.is_empty() {
            return;
        }
        tracing::info!("\n--- Permanently dropped transactions ---");
        for r in &self.replacements {
            tracing::info!(
                "{} -> {} (descendants pruned: {})",
                r.original_txid,
                r.replacement_txid,
                r.pruned_descendants.len()
            );
        }
    }

    fn zero_reason(&self) -> &'static str {
        if !self.wallet_resolved {
            "no wallet loaded on the reorg node"
        } else if self.wallet_txs_seen == 0 {
            "no wallet txs in the orphaned window (wrong spam engine? the default USE_RAW_TX_SPAM=true produces none)"
        } else {
            "all wallet txs in the window were non-root descendants or could not be re-signed"
        }
    }
}

/// Number of eligible txs to permanently drop for a given percentage. `0` when
/// the percentage or the eligible set is empty; otherwise `max(1, floor(n *
/// pct / 100))`, so a small percentage of a small set never silently rounds to
/// a no-op. Pure and deterministic.
pub fn selected_count(eligible: usize, pct: u8) -> usize {
    if eligible == 0 || pct == 0 {
        return 0;
    }
    let pct = (pct.min(100)) as usize;
    (eligible * pct / 100).max(1)
}

/// Flatten the orphaned branch into candidate txids, oldest block first and in
/// mined order within a block, dropping each block's coinbase (index 0).
/// Deterministic: candidate order follows block order, never a txid sort. Pure.
pub fn flatten_branch(branch: &[BranchBlock]) -> Vec<Txid> {
    branch
        .iter()
        .flat_map(|block| block.txids.iter().skip(1).copied())
        .collect()
}

/// The set of mempool txids that must be kept out of the replacement blocks:
/// every selected original plus each original's mempool descendants (mining a
/// descendant of a deliberately-replaced tx would make the block invalid).
/// Pure.
pub fn exclusion_set(replacements: &[ReplacementTx]) -> HashSet<Txid> {
    let mut excluded = HashSet::new();
    for r in replacements {
        excluded.insert(r.original_txid);
        excluded.extend(r.pruned_descendants.iter().copied());
    }
    excluded
}

/// Build the permanent-drop plan for the just-invalidated branch. Never fails
/// the reorg: a tx that cannot be resolved, is not the wallet's, is not a root,
/// or cannot be re-signed is simply skipped. Returns an empty plan (with a
/// loggable reason) when `pct == 0` or nothing is eligible.
pub fn build_plan(node: &Client, branch: &[BranchBlock], pct: u8) -> DoubleSpendPlan {
    if pct == 0 {
        return DoubleSpendPlan::empty(pct, true, 0, 0);
    }

    let Some((wallet_name, wallet)) = resolve_wallet(node) else {
        return DoubleSpendPlan::empty(pct, false, 0, 0);
    };

    if let (Some(first), Some(last)) = (branch.first(), branch.last()) {
        tracing::debug!(
            "double-spend: scanning orphaned branch heights {} ({}) ..= {} ({})",
            first.height,
            first.hash,
            last.height,
            last.hash
        );
    }

    let mut wallet_txs_seen = 0usize;
    let mut eligible: Vec<Candidate> = Vec::new();
    for txid in flatten_branch(branch) {
        // Only the reorg node's own wallet txs can be re-signed into a conflict.
        if wallet.get_transaction(&txid, None).is_err() {
            continue;
        }
        wallet_txs_seen += 1;
        match build_candidate(node, &wallet, &wallet_name, txid) {
            Ok(Some(candidate)) => eligible.push(candidate),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!("double-spend: skipping {txid} ({error})");
            }
        }
    }

    let eligible_count = eligible.len();
    let take = selected_count(eligible_count, pct);
    if take == 0 {
        return DoubleSpendPlan::empty(pct, true, eligible_count, wallet_txs_seen);
    }

    let mut replacements = Vec::with_capacity(take);
    for candidate in eligible.into_iter().take(take) {
        let pruned_descendants = mempool_descendants(node, candidate.original_txid);
        replacements.push(ReplacementTx {
            original_txid: candidate.original_txid,
            replacement_txid: candidate.replacement_txid,
            raw_hex: candidate.raw_hex,
            pruned_descendants,
        });
    }

    let excluded_mempool_txids = exclusion_set(&replacements);
    DoubleSpendPlan {
        replacements,
        excluded_mempool_txids,
        configured_pct: pct,
        eligible_count,
        wallet_txs_seen,
        wallet_resolved: true,
    }
}

struct Candidate {
    original_txid: Txid,
    replacement_txid: Txid,
    raw_hex: String,
}

/// Try to construct a signed same-input conflict for one orphaned wallet tx.
/// Returns `Ok(None)` when the tx is ineligible (not a root, dust, or the
/// wallet cannot fully sign the conflict) and `Err` only on an RPC failure.
fn build_candidate(
    node: &Client,
    wallet: &Client,
    wallet_name: &str,
    txid: Txid,
) -> Result<Option<Candidate>, bitcoincore_rpc::Error> {
    let original = node.get_raw_transaction(&txid, None)?;

    // Every input must spend a UTXO that exists on the rolled-back chain
    // (include_mempool=false). If any prevout is missing, the tx spends the
    // output of another orphaned tx -- it is a descendant, not a root -- so it
    // is left to die with its ancestor rather than rebuilt independently.
    let mut inputs = Vec::with_capacity(original.input.len());
    let mut input_total: u64 = 0;
    for txin in &original.input {
        let prevout = original_prevout(node, txin)?;
        let Some(value) = prevout else {
            return Ok(None);
        };
        input_total += value;
        inputs.push(CreateRawTransactionInput {
            txid: txin.previous_output.txid,
            vout: txin.previous_output.vout,
            sequence: None,
        });
    }

    // Keep the original absolute fee: pay (input_total - fee) = sum of the
    // original outputs to a single fresh address. Skip if that would be dust.
    let output_total: u64 = original.output.iter().map(|o| o.value.to_sat()).sum();
    if output_total < DUST_SAT || output_total > input_total {
        return Ok(None);
    }

    let address = wallet.get_new_address(None, None)?;
    let address = match simchain_common::require_regtest_address(address) {
        Ok(address) => address,
        Err(error) => {
            tracing::debug!("double-spend: fresh address for {wallet_name} unusable ({error})");
            return Ok(None);
        }
    };

    let mut outs: HashMap<String, Amount> = HashMap::new();
    outs.insert(address.to_string(), Amount::from_sat(output_total));

    let raw_hex = node.create_raw_transaction_hex(&inputs, &outs, None, None)?;
    let signed = wallet.sign_raw_transaction_with_wallet(raw_hex.as_str(), None, None)?;
    if !signed.complete {
        return Ok(None);
    }

    let replacement = match signed.transaction() {
        Ok(tx) => tx,
        Err(error) => {
            tracing::debug!("double-spend: could not decode signed conflict for {txid} ({error})");
            return Ok(None);
        }
    };
    Ok(Some(Candidate {
        original_txid: txid,
        replacement_txid: replacement.compute_txid(),
        raw_hex: bitcoincore_rpc::bitcoin::consensus::encode::serialize_hex(&replacement),
    }))
}

/// Value (sats) of an input's prevout on the rolled-back chain, or `None` if it
/// is not in the confirmed UTXO set (spent, or created by another orphaned tx).
fn original_prevout(
    node: &Client,
    txin: &bitcoincore_rpc::bitcoin::TxIn,
) -> Result<Option<u64>, bitcoincore_rpc::Error> {
    let out = node.get_tx_out(
        &txin.previous_output.txid,
        txin.previous_output.vout,
        Some(false),
    )?;
    Ok(out.map(|o| o.value.to_sat()))
}

/// Mempool descendants of a selected original (excluding itself). Best-effort:
/// an RPC error yields an empty list rather than aborting the reorg.
fn mempool_descendants(node: &Client, txid: Txid) -> Vec<Txid> {
    node.call::<Vec<Txid>>("getmempooldescendants", &[json!(txid.to_string())])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoincore_rpc::bitcoin::{hashes::Hash, BlockHash};

    fn txid(n: u8) -> Txid {
        Txid::from_byte_array([n; 32])
    }

    fn block(height: u64, txids: Vec<Txid>) -> BranchBlock {
        BranchBlock {
            height,
            hash: BlockHash::from_byte_array([height as u8; 32]),
            txids,
        }
    }

    #[test]
    fn selected_count_pct_zero_selects_none() {
        assert_eq!(selected_count(5, 0), 0);
    }

    #[test]
    fn selected_count_positive_pct_selects_at_least_one() {
        // 25% of 3 floors to 0 but must select at least one.
        assert_eq!(selected_count(3, 25), 1);
        assert_eq!(selected_count(5, 50), 2);
    }

    #[test]
    fn selected_count_full_pct_selects_all() {
        assert_eq!(selected_count(4, 100), 4);
    }

    #[test]
    fn selected_count_empty_set_selects_none() {
        assert_eq!(selected_count(0, 100), 0);
    }

    #[test]
    fn flatten_branch_follows_block_order_and_drops_coinbase() {
        // Coinbase (index 0) is dropped; remaining order is mined order across
        // oldest-first blocks, never a txid sort.
        let branch = vec![
            block(10, vec![txid(0), txid(9), txid(3)]),
            block(11, vec![txid(1), txid(2), txid(8)]),
        ];
        assert_eq!(
            flatten_branch(&branch),
            vec![txid(9), txid(3), txid(2), txid(8)]
        );
    }

    #[test]
    fn exclusion_set_covers_originals_and_descendants() {
        let replacements = vec![
            ReplacementTx {
                original_txid: txid(1),
                replacement_txid: txid(100),
                raw_hex: String::new(),
                pruned_descendants: vec![txid(2), txid(3)],
            },
            ReplacementTx {
                original_txid: txid(4),
                replacement_txid: txid(101),
                raw_hex: String::new(),
                pruned_descendants: vec![],
            },
        ];
        let excluded = exclusion_set(&replacements);
        assert_eq!(excluded.len(), 4);
        for expected in [txid(1), txid(2), txid(3), txid(4)] {
            assert!(excluded.contains(&expected));
        }
        // Replacements are mined as raw hex, never filtered from the mempool.
        assert!(!excluded.contains(&txid(100)));
    }
}
