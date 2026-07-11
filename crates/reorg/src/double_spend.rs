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
    /// Block weight of `raw_hex`, so the mining loop can reserve room for the
    /// conflict before packing mempool txs into the same block.
    pub weight: u64,
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

    /// Raw hex of every conflict paired with its block weight, in selection
    /// order, for the mining loop to reserve room and mine each conflict.
    pub fn raw_conflicts(&self) -> Vec<(String, u64)> {
        self.replacements
            .iter()
            .map(|r| (r.raw_hex.clone(), r.weight))
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
    /// Each conflict is verified on-chain first (its output is in the confirmed
    /// UTXO set): `mine_exact`'s recovery ladder can fall back to an empty block
    /// and never land a raw conflict, in which case the original stays in the
    /// mempool and can re-confirm -- so a claimed drop that never mined is
    /// reported as a FAILED drop rather than asserted as permanent.
    pub fn log_dropped(&self, node: &Client) {
        if self.replacements.is_empty() {
            return;
        }
        tracing::info!("\n--- Permanently dropped transactions ---");
        for r in &self.replacements {
            if replacement_confirmed(node, r.replacement_txid) {
                tracing::info!(
                    "{} -> {} (descendants pruned: {})",
                    r.original_txid,
                    r.replacement_txid,
                    r.pruned_descendants.len()
                );
            } else {
                tracing::warn!(
                    "{} NOT dropped: replacement {} never confirmed on the winning chain; \
                     the original stays in the mempool and can re-confirm",
                    r.original_txid,
                    r.replacement_txid
                );
            }
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

    // Assess eligibility cheaply first (no keypool address, no signing), so the
    // count driving selection is known without burning an address and a sign
    // round-trip on every candidate -- only the selected roots get signed below.
    let mut wallet_txs_seen = 0usize;
    let mut roots: Vec<EligibleRoot> = Vec::new();
    for txid in flatten_branch(branch) {
        // Only the reorg node's own wallet txs can be re-signed into a conflict.
        if wallet.get_transaction(&txid, None).is_err() {
            continue;
        }
        wallet_txs_seen += 1;
        match assess_root(node, txid) {
            Ok(Some(root)) => roots.push(root),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!("double-spend: skipping {txid} ({error})");
            }
        }
    }

    let eligible_count = roots.len();
    let take = selected_count(eligible_count, pct);
    if take == 0 {
        return DoubleSpendPlan::empty(pct, true, eligible_count, wallet_txs_seen);
    }

    // Sign only the selected roots, in deterministic order. Signing can still
    // fail (a key the wallet no longer holds), so keep walking the eligible list
    // until `take` conflicts succeed rather than signing all of them up front.
    let mut replacements = Vec::with_capacity(take);
    for root in &roots {
        if replacements.len() == take {
            break;
        }
        match sign_conflict(node, &wallet, &wallet_name, root) {
            Ok(Some(candidate)) => {
                let pruned_descendants = mempool_descendants(node, candidate.original_txid);
                replacements.push(ReplacementTx {
                    original_txid: candidate.original_txid,
                    replacement_txid: candidate.replacement_txid,
                    raw_hex: candidate.raw_hex,
                    weight: candidate.weight,
                    pruned_descendants,
                });
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(
                    "double-spend: could not sign {} ({error})",
                    root.original_txid
                );
            }
        }
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

/// A root orphaned wallet tx that passed the cheap structural checks (all
/// inputs are on-chain UTXOs, single-output value non-dust) and is ready to be
/// signed into a conflict. Holds the resolved inputs + payout value so signing
/// needs no further RPC to reconstruct them.
struct EligibleRoot {
    original_txid: Txid,
    inputs: Vec<CreateRawTransactionInput>,
    output_total: u64,
}

struct Candidate {
    original_txid: Txid,
    replacement_txid: Txid,
    raw_hex: String,
    weight: u64,
}

/// Cheap structural eligibility check for one orphaned wallet tx: is it a
/// re-spendable root with a non-dust payout? Returns the resolved inputs +
/// payout value for later signing, `Ok(None)` when ineligible, and `Err` only
/// on an RPC failure. Touches neither the keypool nor the signer, so it is safe
/// to run on every orphaned tx before selection.
fn assess_root(node: &Client, txid: Txid) -> Result<Option<EligibleRoot>, bitcoincore_rpc::Error> {
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
    // The `output_total > input_total` guard is belt-and-suspenders: any
    // consensus-valid original has fee >= 0, so it only trips on malformed data,
    // never on a real orphaned tx.
    let output_total: u64 = original.output.iter().map(|o| o.value.to_sat()).sum();
    if output_total < DUST_SAT || output_total > input_total {
        return Ok(None);
    }

    Ok(Some(EligibleRoot {
        original_txid: txid,
        inputs,
        output_total,
    }))
}

/// Sign one assessed root into a same-input, single-output conflict paid to a
/// fresh wallet address. Consumes a keypool address and a sign RPC, so it runs
/// only for selected roots. `Ok(None)` when the wallet cannot fully sign it
/// (e.g. a key it no longer holds) and `Err` only on an RPC failure.
fn sign_conflict(
    node: &Client,
    wallet: &Client,
    wallet_name: &str,
    root: &EligibleRoot,
) -> Result<Option<Candidate>, bitcoincore_rpc::Error> {
    let address = wallet.get_new_address(None, None)?;
    let address = match simchain_common::require_regtest_address(address) {
        Ok(address) => address,
        Err(error) => {
            tracing::debug!("double-spend: fresh address for {wallet_name} unusable ({error})");
            return Ok(None);
        }
    };

    let mut outs: HashMap<String, Amount> = HashMap::new();
    outs.insert(address.to_string(), Amount::from_sat(root.output_total));

    let raw_hex = node.create_raw_transaction_hex(&root.inputs, &outs, None, None)?;
    let signed = wallet.sign_raw_transaction_with_wallet(raw_hex.as_str(), None, None)?;
    if !signed.complete {
        return Ok(None);
    }

    let replacement = match signed.transaction() {
        Ok(tx) => tx,
        Err(error) => {
            tracing::debug!(
                "double-spend: could not decode signed conflict for {} ({error})",
                root.original_txid
            );
            return Ok(None);
        }
    };
    Ok(Some(Candidate {
        original_txid: root.original_txid,
        replacement_txid: replacement.compute_txid(),
        raw_hex: bitcoincore_rpc::bitcoin::consensus::encode::serialize_hex(&replacement),
        weight: replacement.weight().to_wu(),
    }))
}

/// True when a mined conflict actually landed on the winning chain: its single
/// output is present in the confirmed UTXO set (`include_mempool=false`). A
/// fresh replacement output is never spent within the reorg window, so `None`
/// (or an RPC error) means the conflict was not mined, not mined-and-spent.
fn replacement_confirmed(node: &Client, replacement_txid: Txid) -> bool {
    matches!(
        node.get_tx_out(&replacement_txid, 0, Some(false)),
        Ok(Some(_))
    )
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
/// an RPC error yields an empty list rather than aborting the reorg, but is
/// logged loudly -- an un-excluded descendant of a replaced original would be
/// carried into a replacement block and rejected as invalid (then dropped by
/// `mine_exact`'s filtered retry), so a silent empty list on the feature's most
/// important correctness condition must not pass unnoticed.
fn mempool_descendants(node: &Client, txid: Txid) -> Vec<Txid> {
    match node.call::<Vec<Txid>>("getmempooldescendants", &[json!(txid.to_string())]) {
        Ok(descendants) => descendants,
        Err(error) => {
            tracing::warn!(
                "double-spend: getmempooldescendants({txid}) failed ({error}); \
                 its descendants will not be excluded and may be dropped from a replacement block"
            );
            Vec::new()
        }
    }
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
                weight: 0,
                pruned_descendants: vec![txid(2), txid(3)],
            },
            ReplacementTx {
                original_txid: txid(4),
                replacement_txid: txid(101),
                raw_hex: String::new(),
                weight: 0,
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
