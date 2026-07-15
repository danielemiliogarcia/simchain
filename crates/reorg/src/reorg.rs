//! Reorganization mechanics exposed as a bounded library operation.

use crate::{
    chain::{
        balanced_weight_budget, branch_to_orphan, last_blocks, live_mempool_weighted, mine_exact,
        pack_by_weight, print_blocks, weight_prefix_len, BlockTx, BLOCK_WEIGHT_BUDGET,
    },
    double_spend::{build_plan, DoubleSpendPlan},
    wallet::inject_transactions,
};
use anyhow::Context;
use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use simchain_common::{config::RpcUrl, create_client};
use std::{
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReorgRequest {
    pub depth: u64,
    pub empty: bool,
    pub adds_new_txs: u64,
    pub double_spend_pct: u8,
}

impl ReorgRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.depth > 0, "reorg depth must be at least 1");
        anyhow::ensure!(
            self.double_spend_pct <= 100,
            "double_spend_pct must be between 0 and 100"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct WitnessTarget {
    pub name: String,
    pub rpc_url: RpcUrl,
    /// Control-plane jobs require a positive convergence witness. The legacy
    /// standalone binary keeps its historical best-effort behavior.
    pub required: bool,
}

#[derive(Clone, Debug)]
pub struct ReorgTarget {
    pub node_name: String,
    pub rpc_url: RpcUrl,
    pub mine_address: Address,
    pub wallet_name: String,
    pub witness: Option<WitnessTarget>,
    pub use_raw_tx_spam: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReorgPhase {
    Connecting,
    Inspecting,
    WaitingForTimestamp,
    Invalidating,
    Invalidated,
    MiningReplacement,
    WaitingForWitness,
    Converged,
    AbortDeferred,
    Recovering,
    RecoveryPending,
    Recovered,
    Completed,
}

impl ReorgPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Inspecting => "inspecting",
            Self::WaitingForTimestamp => "waiting_for_timestamp",
            Self::Invalidating => "invalidating",
            Self::Invalidated => "invalidated",
            Self::MiningReplacement => "mining_replacement",
            Self::WaitingForWitness => "waiting_for_witness",
            Self::Converged => "converged",
            Self::AbortDeferred => "abort_deferred",
            Self::Recovering => "recovering_reorg",
            Self::RecoveryPending => "reorg_recovery_pending",
            Self::Recovered => "reorg_recovered",
            Self::Completed => "completed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReorgProgress {
    pub phase: ReorgPhase,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

pub trait ReorgObserver: Send + Sync {
    fn observe(&self, progress: ReorgProgress);

    fn abort_requested(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopObserver;

impl ReorgObserver for NoopObserver {
    fn observe(&self, _progress: ReorgProgress) {}
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReorgResult {
    pub old_tip_height: u64,
    pub old_tip_hash: String,
    pub new_tip_height: u64,
    pub new_tip_hash: String,
    pub target_height: Option<u64>,
    pub returned_transactions: usize,
    pub replacement_blocks: u64,
    pub extra_blocks: u64,
    pub network_converged: bool,
    pub chain_changed: bool,
    pub aborted: bool,
}

/// Execute one bounded reorg using explicit static connection configuration.
/// The observer is called synchronously from the executor thread.
pub fn run_once(
    target: &ReorgTarget,
    request: &ReorgRequest,
    observer: &dyn ReorgObserver,
) -> anyhow::Result<ReorgResult> {
    request.validate()?;
    emit(
        observer,
        ReorgPhase::Connecting,
        format!("connecting to reorg node '{}'", target.node_name),
        None,
    );
    let node = create_client(&target.rpc_url).context("build reorg node client")?;
    let witness_client = target
        .witness
        .as_ref()
        .map(|witness| {
            create_client(&witness.rpc_url)
                .with_context(|| format!("build witness '{}' client", witness.name))
        })
        .transpose()?;
    let witness = witness_client.as_ref().zip(target.witness.as_ref());
    run_with_clients(&node, witness, target, request, observer)
}

fn run_with_clients(
    node: &Client,
    witness: Option<(&Client, &WitnessTarget)>,
    target: &ReorgTarget,
    request: &ReorgRequest,
    observer: &dyn ReorgObserver,
) -> anyhow::Result<ReorgResult> {
    let tip = node.get_block_count().context("get reorg node height")?;
    let old_tip_hash = node
        .get_best_block_hash()
        .context("get reorg node tip")?
        .to_string();
    emit(
        observer,
        ReorgPhase::Inspecting,
        format!("inspecting depth {} at height {tip}", request.depth),
        Some(json!({"height": tip, "depth": request.depth})),
    );

    if tip < request.depth + 1 {
        tracing::warn!(
            "Chain too short (height {tip}) to reorg {} blocks, skipping",
            request.depth
        );
        emit(
            observer,
            ReorgPhase::Completed,
            "chain is too short; no history was changed".to_string(),
            Some(json!({"height": tip})),
        );
        return Ok(ReorgResult {
            old_tip_height: tip,
            old_tip_hash: old_tip_hash.clone(),
            new_tip_height: tip,
            new_tip_hash: old_tip_hash,
            target_height: None,
            returned_transactions: 0,
            replacement_blocks: 0,
            extra_blocks: 0,
            network_converged: false,
            chain_changed: false,
            aborted: observer.abort_requested(),
        });
    }

    tracing::info!(
        "\n=== Simulating a reorg of the last {} blocks ===\n",
        request.depth
    );
    tracing::info!("--- Last {} blocks BEFORE reorg ---", request.depth + 2);
    let before = last_blocks(node, request.depth + 2)?;
    print_blocks(&before);

    let plan_branch = if !request.empty && request.double_spend_pct > 0 {
        Some(branch_to_orphan(node, request.depth)?)
    } else {
        None
    };
    let target_height = tip - request.depth + 1;
    let target_hash = node.get_block_hash(target_height)?;
    let target_time = node.get_block_info(&target_hash)?.time as u64;

    // Move the timestamp wait before invalidation so an abort or slow wall
    // clock cannot leave the node on a shortened chain unnecessarily.
    if now_secs() <= target_time {
        emit(
            observer,
            ReorgPhase::WaitingForTimestamp,
            "waiting for a distinct replacement-block timestamp".to_string(),
            Some(json!({"invalidated_block_time": target_time})),
        );
    }
    while now_secs() <= target_time {
        if observer.abort_requested() {
            return aborted_before_mutation(node, tip, old_tip_hash, target_height, observer);
        }
        thread::sleep(Duration::from_millis(250));
    }
    if observer.abort_requested() {
        return aborted_before_mutation(node, tip, old_tip_hash, target_height, observer);
    }

    let mempool_before = node.get_raw_mempool()?.len();
    emit(
        observer,
        ReorgPhase::Invalidating,
        format!("invalidating block {target_height} ({target_hash})"),
        Some(json!({"height": target_height, "hash": target_hash.to_string()})),
    );
    node.invalidate_block(&target_hash)?;

    let mempool_after = node.get_raw_mempool()?.len();
    let returned = mempool_after.saturating_sub(mempool_before);
    emit(
        observer,
        ReorgPhase::Invalidated,
        format!("{returned} orphaned transactions returned to the mempool"),
        Some(json!({"returned_transactions": returned, "mempool_size": mempool_after})),
    );
    emit_deferred_abort(observer);

    let blocks_to_mine = request.depth + 1;
    let plan: Option<DoubleSpendPlan> = if request.empty {
        if request.double_spend_pct > 0 {
            tracing::info!(
                "Double-spend mode ignored in empty reorg (configured {})",
                request.double_spend_pct
            );
        }
        for index in 0..blocks_to_mine {
            emit(
                observer,
                ReorgPhase::MiningReplacement,
                format!(
                    "mining empty replacement block {}/{}",
                    index + 1,
                    blocks_to_mine
                ),
                Some(json!({"index": index + 1, "total": blocks_to_mine, "empty": true})),
            );
            mine_exact(node, &target.mine_address, &[])?;
            emit_deferred_abort(observer);
        }
        None
    } else {
        let plan = build_plan(
            node,
            plan_branch.as_deref().unwrap_or(&[]),
            request.double_spend_pct,
            &target.rpc_url,
            &target.wallet_name,
        );
        if request.double_spend_pct > 0 {
            plan.log_selection(target.use_raw_tx_spam);
        }
        if request.adds_new_txs > 0 {
            inject_transactions(
                node,
                request.adds_new_txs,
                &target.rpc_url,
                &target.wallet_name,
            );
        }

        let mut pending = plan.raw_conflicts();
        for index in 0..blocks_to_mine {
            let blocks_left = blocks_to_mine - index;
            let mempool = live_mempool_weighted(node, &plan.excluded_mempool_txids)?;
            let conflict_weights: Vec<u64> = pending.iter().map(|(_, weight)| *weight).collect();
            let all_weights: Vec<u64> = conflict_weights
                .iter()
                .copied()
                .chain(mempool.iter().map(|transaction| transaction.weight))
                .collect();
            let budget = balanced_weight_budget(&all_weights, blocks_left, BLOCK_WEIGHT_BUDGET);
            let conflict_count = weight_prefix_len(&conflict_weights, budget);
            let conflicts: Vec<(String, u64)> = pending.drain(..conflict_count).collect();
            let conflict_weight: u64 = conflicts.iter().map(|(_, weight)| *weight).sum();
            let packed = pack_by_weight(&mempool, budget.saturating_sub(conflict_weight));
            let mempool_weight: u64 = mempool[..packed.len()]
                .iter()
                .map(|transaction| transaction.weight)
                .sum();
            emit(
                observer,
                ReorgPhase::MiningReplacement,
                format!("mining replacement block {}/{}", index + 1, blocks_to_mine),
                Some(json!({
                    "index": index + 1,
                    "total": blocks_to_mine,
                    "conflicts": conflicts.len(),
                    "mempool_transactions": packed.len(),
                    "selected_weight": conflict_weight + mempool_weight,
                    "target_weight": budget
                })),
            );
            let mut items = Vec::with_capacity(conflicts.len() + packed.len());
            items.extend(conflicts.into_iter().map(|(hex, _)| BlockTx::RawHex(hex)));
            items.extend(packed.into_iter().map(BlockTx::Mempool));
            mine_exact(node, &target.mine_address, &items)?;
            emit_deferred_abort(observer);
        }
        if !pending.is_empty() {
            tracing::warn!(
                "{} double-spend conflicts did not fit the replacement blocks",
                pending.len()
            );
        }
        Some(plan)
    };

    let (network_converged, extra_blocks) = match witness {
        Some((client, witness)) => ensure_network_adopts(
            node,
            client,
            witness,
            &target.mine_address,
            10,
            request.empty,
            observer,
        )?,
        None => (false, 0),
    };
    thread::sleep(Duration::from_secs(2));

    tracing::info!("\n--- Last {} blocks AFTER reorg ---", request.depth + 3);
    let after = last_blocks(node, request.depth + 3)?;
    print_blocks(&after);
    if let Some(plan) = &plan {
        plan.log_dropped(node);
    }

    let new_tip_height = node.get_block_count()?;
    let new_tip_hash = node.get_best_block_hash()?.to_string();
    let aborted = observer.abort_requested();
    emit(
        observer,
        ReorgPhase::Completed,
        if aborted {
            "reorg reached a safe converged result after abort was requested".to_string()
        } else {
            format!("reorg completed at height {new_tip_height}")
        },
        Some(json!({
            "height": new_tip_height,
            "hash": new_tip_hash,
            "network_converged": network_converged,
            "abort_requested": aborted
        })),
    );
    Ok(ReorgResult {
        old_tip_height: tip,
        old_tip_hash,
        new_tip_height,
        new_tip_hash,
        target_height: Some(target_height),
        returned_transactions: returned,
        replacement_blocks: blocks_to_mine + extra_blocks,
        extra_blocks,
        network_converged,
        chain_changed: true,
        aborted,
    })
}

fn ensure_network_adopts(
    node: &Client,
    witness: &Client,
    witness_target: &WitnessTarget,
    mine_address: &Address,
    max_extra: u64,
    empty: bool,
    observer: &dyn ReorgObserver,
) -> anyhow::Result<(bool, u64)> {
    for extra in 0..=max_extra {
        let tip = node.get_best_block_hash()?;
        emit(
            observer,
            ReorgPhase::WaitingForWitness,
            format!("waiting for '{}' to adopt {tip}", witness_target.name),
            Some(
                json!({"witness": witness_target.name, "tip": tip.to_string(), "extra_blocks": extra}),
            ),
        );
        for _ in 0..12 {
            match witness.get_best_block_hash() {
                Ok(hash) if hash == tip => {
                    emit(
                        observer,
                        ReorgPhase::Converged,
                        format!("'{}' adopted the replacement chain", witness_target.name),
                        Some(json!({"extra_blocks": extra, "tip": tip.to_string()})),
                    );
                    return Ok((true, extra));
                }
                Ok(_) => thread::sleep(Duration::from_millis(250)),
                Err(error) if witness_target.required => {
                    anyhow::bail!(
                        "required witness '{}' is unavailable: {error}",
                        witness_target.name
                    )
                }
                Err(error) => {
                    tracing::warn!(
                        "Witness '{}' unavailable ({error}); convergence was not verified",
                        witness_target.name
                    );
                    return Ok((false, extra));
                }
            }
        }
        if extra == max_extra {
            break;
        }
        emit_deferred_abort(observer);
        if empty {
            mine_exact(node, mine_address, &[])?;
        } else {
            node.generate_to_address(1, mine_address)?;
        }
    }
    if witness_target.required {
        anyhow::bail!(
            "required witness '{}' did not converge after {max_extra} extra blocks",
            witness_target.name
        );
    }
    tracing::warn!("network convergence was not observed after {max_extra} extra blocks");
    Ok((false, max_extra))
}

fn aborted_before_mutation(
    node: &Client,
    old_tip_height: u64,
    old_tip_hash: String,
    target_height: u64,
    observer: &dyn ReorgObserver,
) -> anyhow::Result<ReorgResult> {
    let result = ReorgResult {
        old_tip_height,
        old_tip_hash: old_tip_hash.clone(),
        new_tip_height: node.get_block_count()?,
        new_tip_hash: node.get_best_block_hash()?.to_string(),
        target_height: Some(target_height),
        returned_transactions: 0,
        replacement_blocks: 0,
        extra_blocks: 0,
        network_converged: false,
        chain_changed: false,
        aborted: true,
    };
    emit(
        observer,
        ReorgPhase::Completed,
        "reorg aborted before history mutation".to_string(),
        Some(json!({"height": result.new_tip_height, "abort_requested": true})),
    );
    Ok(result)
}

fn emit_deferred_abort(observer: &dyn ReorgObserver) {
    if observer.abort_requested() {
        emit(
            observer,
            ReorgPhase::AbortDeferred,
            "abort requested after history mutation; finishing the safe rewrite".to_string(),
            None,
        );
    }
}

fn emit(observer: &dyn ReorgObserver, phase: ReorgPhase, message: String, data: Option<Value>) {
    observer.observe(ReorgProgress {
        phase,
        message,
        data,
    });
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation_rejects_unsafe_bounds() {
        let valid = ReorgRequest {
            depth: 3,
            empty: true,
            adds_new_txs: 0,
            double_spend_pct: 50,
        };
        assert!(valid.validate().is_ok());
        assert!(ReorgRequest {
            depth: 0,
            ..valid.clone()
        }
        .validate()
        .is_err());
        assert!(ReorgRequest {
            double_spend_pct: 101,
            ..valid
        }
        .validate()
        .is_err());
    }
}
