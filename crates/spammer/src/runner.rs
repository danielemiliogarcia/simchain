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
use simchain_common::live_tuning::SpamTuning;
use simchain_common::{create_client, create_jsonrpc_client, create_wallet_client};
use std::{thread, time::Duration};

enum SpamEngine {
    Raw {
        node2: Box<RawSpammer>,
        node3: Box<RawSpammer>,
    },
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
            set_wallet_fees(
                &wallet2,
                &config.wallet2_name,
                &wallet3,
                &config.wallet3_name,
                policy.fallback_fee,
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
    let mut spammed_at_height = 0u64;

    loop {
        match control.safe_point() {
            SafePointAction::Initialize { policy, .. } => {
                match SpamEngine::build(config, &policy, None, false) {
                    Ok(new_engine) => {
                        engine = Some(new_engine);
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
                policy, rebuild, ..
            } => {
                let result = if !policy.enabled {
                    engine = None;
                    Ok(())
                } else if rebuild || engine.is_none() {
                    let previous_fee = Some(configured_fee(&control));
                    let previous_uses_wallet_fee =
                        matches!(engine.as_ref(), Some(SpamEngine::Wallet { .. }));
                    match SpamEngine::build(config, &policy, previous_fee, previous_uses_wallet_fee)
                    {
                        Ok(new_engine) => {
                            engine = Some(new_engine);
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
                } else {
                    Ok(())
                };
                if let Err(error) = &result {
                    tracing::warn!("spam policy engine rebuild rejected: {error}");
                }
                control.complete_policy(result, engine.is_some());
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
                if current_height > spammed_at_height
                    && control.begin_cycle(generation, current_height)
                {
                    spammed_at_height = current_height;
                    let cycle_start = std::time::Instant::now();
                    let result = engine
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("spam engine is unavailable"))
                        .and_then(|engine| engine.run_cycle(&node1, &policy, generation, &control));
                    let accepted = match result {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            control.record_error(error.to_string());
                            tracing::warn!("spam cycle failed: {error}");
                            0
                        }
                    };
                    control.finish_cycle(current_height, accepted);
                    tracing::info!(
                        "Spam cycle done in {:.1}s ({accepted} txs accepted)",
                        cycle_start.elapsed().as_secs_f32()
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

fn configured_fee(control: &SpamControl) -> f64 {
    control.status().policy.fallback_fee
}

fn set_wallet_tx_fee(wallet: &Client, name: &str, fee_btc_per_kvb: f64) -> anyhow::Result<()> {
    let accepted = wallet
        .call::<bool>("settxfee", &[json!(fee_btc_per_kvb)])
        .with_context(|| format!("settxfee {fee_btc_per_kvb} on wallet '{name}' failed"))?;
    anyhow::ensure!(
        accepted,
        "wallet '{name}' rejected settxfee {fee_btc_per_kvb}"
    );
    tracing::info!("Wallet '{name}' paytxfee pinned to {fee_btc_per_kvb} BTC/kvB (FALLBACK_FEE)");
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

fn effective_fanout(policy: &SpamTuning) -> u64 {
    if policy.fanout_auto {
        std::cmp::max(12, (policy.fill_block_ratio * 15.0).ceil() as u64)
    } else {
        policy.fanout_utxos
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
    let fanout = effective_fanout(policy);
    let small2 = policy.small_txs_per_block.div_ceil(MINER_COUNT);
    let small3 = policy.small_txs_per_block / MINER_COUNT;
    let pool2 = policy.floor_pool_txs.div_ceil(MINER_COUNT);
    let pool3 = policy.floor_pool_txs / MINER_COUNT;
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
            let fills = node2.floor_round(pool2, &checkpoint);
            let (txids, _) = node2.hybrid_round(
                deficit2,
                small2,
                fanout,
                policy.enable_replaces,
                policy.replaces_per_miner,
                &checkpoint,
            );
            fills + txids.len()
        });
        let worker3 = scope.spawn(|| {
            let checkpoint = |phase: &str| control.cycle_checkpoint(generation, phase);
            let fills = node3.floor_round(pool3, &checkpoint);
            let (txids, _) = node3.hybrid_round(
                deficit3,
                small3,
                fanout,
                policy.enable_replaces,
                policy.replaces_per_miner,
                &checkpoint,
            );
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
