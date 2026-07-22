//! Resident spam worker startup, engine ownership, and cycle coordination.

use crate::{
    burn::{burn_address, MINER_COUNT},
    config::SpamConfig,
    control::{SafePointAction, SpamControl, WorkerWait},
    node_wallet_spammer,
    raw_transaction_spammer::RawSpammer,
};
use anyhow::Context;
use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use serde_json::json;
use simchain_common::internal_api::{SpamCapacityState, SpamCapacityStatus};
use simchain_common::live_tuning::SpamTuning;
use simchain_common::{create_client, create_jsonrpc_client, create_wallet_client};
use std::{thread, time::Duration};

enum SpamEngine {
    Raw {
        node2: Box<RawSpammer>,
        node3: Box<RawSpammer>,
    },
    // DEPRECATED and unreachable in practice: USE_RAW_TX_SPAM is pinned to
    // true, so policies always select the raw engine. Sends through the miner
    // nodes' wallets, putting coin-selection and signing load on nodes that
    // should stay light to mine blocks. See node_wallet_spammer.rs.
    Wallet {
        wallet2: Client,
        wallet3: Client,
        sequential_address: Address,
        batch_addresses: Vec<Address>,
    },
}

struct EngineBuildError {
    error: anyhow::Error,
    previous_engine_usable: bool,
}

impl EngineBuildError {
    fn safe(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            previous_engine_usable: true,
        }
    }
}

impl SpamEngine {
    fn build(
        config: &SpamConfig,
        policy: &SpamTuning,
        previous_wallet_fee: Option<f64>,
        previous_engine_uses_wallet_fee: bool,
    ) -> Result<Self, EngineBuildError> {
        let wallet2 = create_wallet_client(&config.node2_url, &config.wallet2_name)
            .context("build node2 wallet client")
            .map_err(EngineBuildError::safe)?;
        let wallet3 = create_wallet_client(&config.node3_url, &config.wallet3_name)
            .context("build node3 wallet client")
            .map_err(EngineBuildError::safe)?;

        if policy.use_raw {
            let mut node2 = RawSpammer::new(
                create_client(&config.node2_url).map_err(EngineBuildError::safe)?,
                create_jsonrpc_client(&config.node2_url).map_err(EngineBuildError::safe)?,
                vec![create_jsonrpc_client(&config.node3_url).map_err(EngineBuildError::safe)?],
                wallet2,
                &config.wallet2_name,
                // Key namespace: keep the pre-namespace derivation so a
                // restarted spammer recovers its existing coins.
                &config.wallet2_name,
                "Node 2",
                policy.fee_rate_sat_vb(),
                policy.sendmany_outputs,
                policy.effective_data_min_bytes(),
                policy.data_max_bytes,
            );
            let mut node3 = RawSpammer::new(
                create_client(&config.node3_url).map_err(EngineBuildError::safe)?,
                create_jsonrpc_client(&config.node3_url).map_err(EngineBuildError::safe)?,
                vec![create_jsonrpc_client(&config.node2_url).map_err(EngineBuildError::safe)?],
                wallet3,
                &config.wallet3_name,
                &config.wallet3_name,
                "Node 3",
                policy.fee_rate_sat_vb(),
                policy.sendmany_outputs,
                policy.effective_data_min_bytes(),
                policy.data_max_bytes,
            );
            node2
                .reconcile()
                .context("reconcile node2 raw engine")
                .map_err(EngineBuildError::safe)?;
            node3
                .reconcile()
                .context("reconcile node3 raw engine")
                .map_err(EngineBuildError::safe)?;
            Ok(Self::Raw {
                node2: Box::new(node2),
                node3: Box::new(node3),
            })
        } else {
            tracing::warn!(
                "USE_RAW_TX_SPAM=false selects the deprecated node-wallet engine: coin selection and signing run inside the miner nodes, which should stay light to mine blocks"
            );
            set_wallet_fees(
                &wallet2,
                &config.wallet2_name,
                &wallet3,
                &config.wallet3_name,
                policy.spam_fee,
                previous_wallet_fee,
                previous_engine_uses_wallet_fee,
            )?;
            Ok(Self::Wallet {
                wallet2,
                wallet3,
                sequential_address: burn_address(0),
                batch_addresses: (1..=policy.sendmany_outputs).map(burn_address).collect(),
            })
        }
    }

    fn reconcile(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Raw { node2, node3 } => {
                node2.reconcile().context("reconcile node2 raw engine")?;
                node3.reconcile().context("reconcile node3 raw engine")?;
                Ok(())
            }
            // The wallet engine delegates coin selection and transaction
            // history to bitcoind and has no local chain-derived state.
            Self::Wallet { .. } => Ok(()),
        }
    }

    fn apply_policy(&mut self, policy: &SpamTuning) -> anyhow::Result<()> {
        match self {
            Self::Raw { node2, node3 } if policy.use_raw => {
                let prepared = RawSpammer::prepare_policy(policy);
                node2.apply_prepared_policy(&prepared);
                node3.apply_prepared_policy(&prepared);
                Ok(())
            }
            Self::Wallet { .. } if !policy.use_raw => {
                anyhow::bail!("the deprecated wallet engine cannot be hot-retuned")
            }
            _ => anyhow::bail!("installed spam engine does not match requested policy"),
        }
    }

    fn capacity(&self, policy: &SpamTuning) -> Option<SpamCapacityStatus> {
        let Self::Raw { node2, node3 } = self else {
            return None;
        };
        let left = node2.capacity();
        let right = node3.capacity();
        let usable = left.usable_branches.min(right.usable_branches);
        let (required, target) = if policy.data_max_bytes > 0 {
            (policy.minimum_data_fanout(), policy.desired_data_fanout())
        } else {
            let (left_share, right_share) = SpamConfig::fixed_shares(policy);
            (
                left_share.max(right_share).min(policy.fanout_utxos.max(1)),
                policy.fanout_utxos.max(1),
            )
        };
        let provisioning = left.branch_provisioning || right.branch_provisioning;
        let state = if usable < required {
            SpamCapacityState::CapacityDegraded
        } else if provisioning || usable < target {
            SpamCapacityState::Provisioning
        } else {
            SpamCapacityState::Ready
        };
        Some(SpamCapacityStatus {
            state,
            usable_branches_per_miner: usable,
            required_branches_per_miner: required,
            target_branches_per_miner: target,
            branch_provisioning: provisioning,
            floor_pool_provisioning: left.floor_pool_provisioning || right.floor_pool_provisioning,
        })
    }

    fn run_cycle(
        &mut self,
        node1: &Client,
        policy: &SpamTuning,
        generation: u64,
        control: &SpamControl,
    ) -> anyhow::Result<usize> {
        match self {
            Self::Raw { node2, node3 } if policy.use_raw && policy.data_max_bytes > 0 => Ok(
                run_raw_data_cycle(node1, node2, node3, policy, generation, control),
            ),
            Self::Raw { node2, node3 } if policy.use_raw => Ok(run_raw_output_cycle(
                node2, node3, policy, generation, control,
            )),
            Self::Wallet {
                wallet2,
                wallet3,
                sequential_address,
                batch_addresses,
            } if !policy.use_raw => Ok(run_wallet_cycle(
                wallet2,
                wallet3,
                sequential_address,
                batch_addresses,
                policy,
                generation,
                control,
            )),
            _ => anyhow::bail!("installed spam engine does not match effective policy"),
        }
    }
}

/// Start the private control server and keep the spam process resident even
/// when its effective policy is disabled.
pub fn run() -> anyhow::Result<()> {
    let config = SpamConfig::global();
    let control = SpamControl::new(config.initial_policy.clone());
    let _control_server = crate::server::spawn(
        config.control_listen_addr,
        config.internal_token.clone(),
        control.clone(),
    )?;
    let node1 = create_client(&config.node1_url).context("build node1 client")?;
    let mut engine: Option<SpamEngine> = None;
    let mut engine_has_initialized = false;
    let mut spammed_at_height = 0u64;
    let mut catch_up_request = None;
    let mut last_cycle_policy: Option<SpamTuning> = None;

    loop {
        match control.safe_point() {
            SafePointAction::Initialize { policy, .. } => {
                match SpamEngine::build(config, &policy, None, false) {
                    Ok(new_engine) => {
                        engine = Some(new_engine);
                        if engine_has_initialized {
                            control.record_recovery("engine reconstructed after loss");
                        }
                        engine_has_initialized = true;
                        report_capacity(&control, engine.as_ref(), &policy);
                        control.complete_initialization(Ok(()));
                    }
                    Err(failure) => {
                        tracing::warn!("spam engine initialization failed: {}", failure.error);
                        control.complete_initialization(Err(failure.error));
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
            SafePointAction::ApplyPolicy {
                generation,
                policy,
                impact,
            } => {
                let result = if impact.engine_changed || (policy.enabled && engine.is_none()) {
                    let engine_was_missing = engine.is_none();
                    let previous_fee = Some(configured_fee(&control));
                    let previous_uses_wallet_fee =
                        matches!(engine.as_ref(), Some(SpamEngine::Wallet { .. }));
                    match SpamEngine::build(config, &policy, previous_fee, previous_uses_wallet_fee)
                    {
                        Ok(new_engine) => {
                            engine = Some(new_engine);
                            if engine_was_missing && engine_has_initialized {
                                control.record_recovery("engine reconstructed after loss");
                            }
                            engine_has_initialized = true;
                            Ok(())
                        }
                        Err(failure) => {
                            if !failure.previous_engine_usable {
                                // Do not resume a wallet engine whose fee rollback
                                // could not be completed. The control state queues
                                // initialization of the still-effective old policy.
                                engine = None;
                            }
                            Err(failure.error)
                        }
                    }
                } else if let Some(engine) = engine.as_mut() {
                    engine.apply_policy(&policy)
                } else {
                    Ok(())
                };
                let applied = result.is_ok();
                if let Err(error) = &result {
                    tracing::warn!("spam policy apply rejected: {error}");
                }
                control.complete_policy(result, engine.is_some());
                if applied {
                    catch_up_request = impact.needs_immediate_cycle.then_some(CatchUpRequest {
                        generation,
                        height: spammed_at_height,
                    });
                }
                if applied {
                    report_capacity(&control, engine.as_ref(), &policy);
                }
            }
            SafePointAction::Reconcile => {
                let result = engine
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("spam engine is unavailable"))
                    .and_then(SpamEngine::reconcile);
                if result.is_ok() {
                    if let Ok(height) = node1.get_block_count() {
                        spammed_at_height = height.saturating_sub(1);
                    }
                } else if let Err(error) = &result {
                    tracing::warn!("spam reconciliation failed: {error}");
                }
                control.complete_reconciliation(result);
                report_capacity(&control, engine.as_ref(), &control.status().policy);
                if control.status().reconciliation_pending {
                    thread::sleep(Duration::from_millis(500));
                }
            }
            SafePointAction::Ready { generation, policy } => {
                let current_height = match node1.get_block_count() {
                    Ok(height) => height,
                    Err(error) => {
                        control.record_error(format!("get node1 block count: {error}"));
                        let _ = control.wait_for_block_poll(Duration::from_millis(500), generation);
                        continue;
                    }
                };
                let cycle_kind = cycle_kind(
                    current_height,
                    spammed_at_height,
                    catch_up_request,
                    generation,
                );
                if catch_up_superseded(
                    catch_up_request,
                    generation,
                    current_height,
                    spammed_at_height,
                ) {
                    catch_up_request = None;
                }
                if cycle_kind != CycleKind::NotDue
                    && control.begin_cycle(generation, current_height)
                {
                    if cycle_kind == CycleKind::CatchUp {
                        catch_up_request = None;
                    }
                    spammed_at_height = current_height;
                    let cycle_policy = if cycle_kind == CycleKind::CatchUp {
                        catch_up_policy(&policy, last_cycle_policy.as_ref())
                    } else {
                        policy.clone()
                    };
                    let cycle_start = std::time::Instant::now();
                    let result = engine
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("spam engine is unavailable"))
                        .and_then(|engine| {
                            engine.run_cycle(&node1, &cycle_policy, generation, &control)
                        });
                    let accepted = match result {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            control.record_error(error.to_string());
                            tracing::warn!("spam cycle failed: {error}");
                            0
                        }
                    };
                    let cycle_duration = cycle_start.elapsed();
                    control.finish_cycle(current_height, accepted, cycle_duration);
                    last_cycle_policy = Some(policy.clone());
                    report_capacity(&control, engine.as_ref(), &policy);
                    tracing::info!(
                        "Spam cycle done in {:.1}s ({accepted} txs accepted)",
                        cycle_duration.as_secs_f32()
                    );
                }
                if control.wait_for_block_poll(Duration::from_millis(200), generation)
                    == WorkerWait::Interrupted
                {
                    continue;
                }
            }
        }
    }
}

fn report_capacity(control: &SpamControl, engine: Option<&SpamEngine>, policy: &SpamTuning) {
    if let Some(capacity) = engine.and_then(|engine| engine.capacity(policy)) {
        control.report_capacity(capacity);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CatchUpRequest {
    generation: u64,
    height: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CycleKind {
    NotDue,
    Normal,
    CatchUp,
}

fn cycle_kind(
    current_height: u64,
    spammed_at_height: u64,
    catch_up: Option<CatchUpRequest>,
    generation: u64,
) -> CycleKind {
    if current_height > spammed_at_height {
        CycleKind::Normal
    } else if current_height == spammed_at_height
        && catch_up.is_some_and(|request| {
            request.generation == generation && request.height == current_height
        })
    {
        CycleKind::CatchUp
    } else {
        CycleKind::NotDue
    }
}

fn catch_up_superseded(
    catch_up: Option<CatchUpRequest>,
    generation: u64,
    current_height: u64,
    spammed_at_height: u64,
) -> bool {
    catch_up.is_some_and(|request| {
        request.generation != generation
            || request.height != current_height
            || current_height != spammed_at_height
    })
}

fn catch_up_policy(next: &SpamTuning, previous: Option<&SpamTuning>) -> SpamTuning {
    let Some(previous) = previous else {
        return next.clone();
    };
    let mut policy = next.clone();
    match (previous.data_max_bytes > 0, next.data_max_bytes > 0) {
        (false, false) => {
            policy.fixed_txs_per_block = next
                .fixed_txs_per_block
                .saturating_sub(previous.fixed_txs_per_block);
        }
        (true, true) => {
            policy.small_txs_per_block = next
                .small_txs_per_block
                .saturating_sub(previous.small_txs_per_block);
        }
        _ => {}
    }
    policy
}

fn configured_fee(control: &SpamControl) -> f64 {
    control.status().policy.spam_fee
}

fn set_wallet_tx_fee(wallet: &Client, name: &str, fee_btc_per_kvb: f64) -> anyhow::Result<()> {
    let accepted = wallet
        .call::<bool>("settxfee", &[json!(fee_btc_per_kvb)])
        .with_context(|| format!("settxfee {fee_btc_per_kvb} on wallet '{name}' failed"))?;
    anyhow::ensure!(
        accepted,
        "wallet '{name}' rejected settxfee {fee_btc_per_kvb}"
    );
    tracing::info!("Wallet '{name}' paytxfee pinned to {fee_btc_per_kvb} BTC/kvB (SPAM_FEE)");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn set_wallet_fees(
    wallet2: &Client,
    name2: &str,
    wallet3: &Client,
    name3: &str,
    new_fee: f64,
    previous_fee: Option<f64>,
    previous_engine_uses_wallet_fee: bool,
) -> Result<(), EngineBuildError> {
    let apply = set_wallet_tx_fee(wallet2, name2, new_fee)
        .and_then(|()| set_wallet_tx_fee(wallet3, name3, new_fee));
    let Err(error) = apply else {
        return Ok(());
    };

    let fee_changed = previous_fee.is_some_and(|previous| previous != new_fee);
    if !previous_engine_uses_wallet_fee || !fee_changed {
        return Err(EngineBuildError::safe(error));
    }

    let previous = previous_fee.expect("fee change has a previous value");
    let restore2 = set_wallet_tx_fee(wallet2, name2, previous);
    // Attempt both restores even if node2 is unavailable: node3 may have
    // accepted the timed-out request that surfaced as the original error.
    let restore3 = set_wallet_tx_fee(wallet3, name3, previous);
    match (restore2, restore3) {
        (Ok(()), Ok(())) => Err(EngineBuildError::safe(error)),
        (left, right) => {
            let mut failures = Vec::new();
            if let Err(restore_error) = left {
                failures.push(format!("{name2}: {restore_error}"));
            }
            if let Err(restore_error) = right {
                failures.push(format!("{name3}: {restore_error}"));
            }
            Err(EngineBuildError {
                error: error.context(format!(
                    "previous wallet fee could not be fully restored ({})",
                    failures.join("; ")
                )),
                previous_engine_usable: false,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FanoutCapacity {
    minimum: u64,
    target: u64,
}

fn data_fanout_capacity(policy: &SpamTuning) -> FanoutCapacity {
    FanoutCapacity {
        minimum: policy.minimum_data_fanout(),
        target: policy.desired_data_fanout(),
    }
}

fn effective_floor_pool_txs(policy: &SpamTuning) -> u64 {
    if policy.fill_block_ratio >= 1.0 {
        policy.floor_pool_txs
    } else {
        0
    }
}

fn run_raw_data_cycle(
    node1: &Client,
    node2: &mut RawSpammer,
    node3: &mut RawSpammer,
    policy: &SpamTuning,
    generation: u64,
    control: &SpamControl,
) -> usize {
    const BLOCK_VSIZE: u64 = 1_000_000;
    let fanout = data_fanout_capacity(policy);
    let small2 = policy.small_txs_per_block.div_ceil(MINER_COUNT);
    let small3 = policy.small_txs_per_block / MINER_COUNT;
    // A sub-block ratio intentionally models partial blocks. Maintaining the
    // standing floor pool there would add up to another block of traffic and
    // make the configured ratio impossible to observe.
    let floor_pool_txs = effective_floor_pool_txs(policy);
    let pool2 = floor_pool_txs.div_ceil(MINER_COUNT);
    let pool3 = floor_pool_txs / MINER_COUNT;
    let mempool = node1
        .get_mempool_info()
        .map(|info| info.bytes as u64)
        .unwrap_or(0);
    let reserve = if policy.fill_block_ratio >= 1.0 {
        BLOCK_VSIZE / 10
    } else {
        0
    };
    let target = (policy.fill_block_ratio * BLOCK_VSIZE as f64) as u64 + reserve;
    let deficit = target.saturating_sub(mempool);
    let deficit2 = deficit / MINER_COUNT;
    let deficit3 = deficit - deficit2;

    let (accepted2, accepted3) = thread::scope(|scope| {
        let worker2 = scope.spawn(|| {
            let checkpoint = |phase: &str| control.cycle_checkpoint(generation, phase);
            let (txids, _) = node2.hybrid_round(
                deficit2,
                small2,
                (fanout.minimum, fanout.target),
                policy.enable_replaces,
                policy.replaces_per_miner,
                &checkpoint,
            );
            // Protect block fullness first. A cold floor pool can require
            // thousands of small RPC submissions, while bulk DATA traffic can
            // establish the requested multi-block backlog quickly.
            let fills = node2.floor_round(pool2, &checkpoint);
            fills + txids.len()
        });
        let worker3 = scope.spawn(|| {
            let checkpoint = |phase: &str| control.cycle_checkpoint(generation, phase);
            let (txids, _) = node3.hybrid_round(
                deficit3,
                small3,
                (fanout.minimum, fanout.target),
                policy.enable_replaces,
                policy.replaces_per_miner,
                &checkpoint,
            );
            let fills = node3.floor_round(pool3, &checkpoint);
            fills + txids.len()
        });
        (
            worker2.join().expect("node2 spam thread panicked"),
            worker3.join().expect("node3 spam thread panicked"),
        )
    });
    accepted2 + accepted3
}

fn run_raw_output_cycle(
    node2: &mut RawSpammer,
    node3: &mut RawSpammer,
    policy: &SpamTuning,
    generation: u64,
    control: &SpamControl,
) -> usize {
    let fanout = policy.fanout_utxos.max(1);
    let (fixed2, fixed3) = SpamConfig::fixed_shares(policy);
    let (txids2, txids3) = thread::scope(|scope| {
        let worker2 = scope.spawn(|| {
            let checkpoint = |phase: &str| control.cycle_checkpoint(generation, phase);
            node2.output_round(
                fixed2,
                fanout,
                policy.enable_replaces,
                policy.replaces_per_miner,
                &checkpoint,
            )
        });
        let worker3 = scope.spawn(|| {
            let checkpoint = |phase: &str| control.cycle_checkpoint(generation, phase);
            node3.output_round(
                fixed3,
                fanout,
                policy.enable_replaces,
                policy.replaces_per_miner,
                &checkpoint,
            )
        });
        (
            worker2.join().expect("node2 spam thread panicked"),
            worker3.join().expect("node3 spam thread panicked"),
        )
    });
    txids2.len() + txids3.len()
}

#[allow(clippy::too_many_arguments)]
fn run_wallet_cycle(
    wallet2: &Client,
    wallet3: &Client,
    sequential_address: &Address,
    batch_addresses: &[Address],
    policy: &SpamTuning,
    generation: u64,
    control: &SpamControl,
) -> usize {
    let (fixed2, fixed3) = SpamConfig::fixed_shares(policy);
    let fanout_need = fixed2.min(policy.fanout_utxos);
    let config = SpamConfig::global();
    let (txids2, txids3) = thread::scope(|scope| {
        let worker2 = scope.spawn(|| {
            let checkpoint = |phase: &str| control.cycle_checkpoint(generation, phase);
            node_wallet_spammer::spam_round(
                wallet2,
                &config.wallet2_name,
                "Node 2",
                fixed2,
                fanout_need,
                policy.fanout_utxos,
                sequential_address,
                batch_addresses,
                policy.enable_replaces,
                policy.replaces_per_miner,
                &checkpoint,
            )
        });
        let worker3 = scope.spawn(|| {
            let checkpoint = |phase: &str| control.cycle_checkpoint(generation, phase);
            node_wallet_spammer::spam_round(
                wallet3,
                &config.wallet3_name,
                "Node 3",
                fixed3,
                fanout_need,
                policy.fanout_utxos,
                sequential_address,
                batch_addresses,
                policy.enable_replaces,
                policy.replaces_per_miner,
                &checkpoint,
            )
        });
        (
            worker2.join().expect("node2 spam thread panicked"),
            worker3.join().expect("node3 spam thread panicked"),
        )
    });
    txids2.len() + txids3.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use simchain_common::live_tuning;
    use std::collections::BTreeMap;

    fn policy() -> SpamTuning {
        SpamTuning::from_source(&live_tuning::staged_map(&BTreeMap::new()))
            .expect("default spam policy")
            .0
    }

    #[test]
    fn catch_up_can_run_once_at_the_current_height() {
        let request = Some(CatchUpRequest {
            generation: 7,
            height: 100,
        });
        assert_eq!(cycle_kind(100, 100, None, 7), CycleKind::NotDue);
        assert_eq!(cycle_kind(100, 100, request, 7), CycleKind::CatchUp);
        assert!(!catch_up_superseded(request, 7, 100, 100));
        assert_eq!(cycle_kind(101, 100, None, 7), CycleKind::Normal);
    }

    #[test]
    fn catch_up_surviving_failed_begin_is_dropped_at_a_new_height() {
        let request = Some(CatchUpRequest {
            generation: 7,
            height: 100,
        });

        // A same-height begin_cycle rejection leaves the request available for
        // retry after the concurrent pause or lease clears.
        assert_eq!(cycle_kind(100, 100, request, 7), CycleKind::CatchUp);
        assert!(!catch_up_superseded(request, 7, 100, 100));

        // If a block arrives first, the request is stale. The fresh height gets
        // one full normal cycle and cannot run the old delta on its next poll.
        assert_eq!(cycle_kind(101, 100, request, 7), CycleKind::Normal);
        assert!(catch_up_superseded(request, 7, 101, 100));
        assert_eq!(cycle_kind(101, 101, None, 7), CycleKind::NotDue);
    }

    #[test]
    fn modest_ratio_increase_uses_existing_headroom() {
        let mut changed = policy();
        changed.fill_block_ratio = 5.0;
        let capacity = data_fanout_capacity(&changed);
        assert_eq!(capacity.minimum, 50);
        assert_eq!(capacity.target, 75);
        assert!(60 >= capacity.minimum);
        assert!(60 < capacity.target);
    }

    #[test]
    fn partial_fill_ratio_disables_floor_pool_replenishment() {
        let mut changed = policy();
        changed.floor_pool_txs = 4_000;
        changed.fill_block_ratio = 0.5;
        assert_eq!(effective_floor_pool_txs(&changed), 0);

        changed.fill_block_ratio = 1.0;
        assert_eq!(effective_floor_pool_txs(&changed), 4_000);
    }

    #[test]
    fn same_height_catch_up_uses_count_deltas() {
        let mut previous = policy();
        previous.data_max_bytes = 0;
        previous.data_min_bytes = 0;
        previous.fixed_txs_per_block = 100;
        let mut next = previous.clone();
        next.fixed_txs_per_block = 140;
        assert_eq!(
            catch_up_policy(&next, Some(&previous)).fixed_txs_per_block,
            40
        );

        previous.data_max_bytes = 1_000;
        previous.small_txs_per_block = 10;
        next = previous.clone();
        next.small_txs_per_block = 25;
        assert_eq!(
            catch_up_policy(&next, Some(&previous)).small_txs_per_block,
            15
        );
    }
}
