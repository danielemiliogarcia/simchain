//! Production adapter from the control-plane reorg request to the reusable
//! reorg library. Tests replace this narrow executor, never Bitcoin RPC.

use crate::state::ControlPlaneConfig;
use bitcoincore_rpc::bitcoin::{address::NetworkUnchecked, Address};
use bitcoincore_rpc::{Client, RpcApi};
use serde::{Deserialize, Serialize};
use serde_json::json;
use simchain_common::config::{
    parse_rpc_url, RpcUrl, DEFAULT_NODE2_WALLET_NAME, DEFAULT_NODE3_WALLET_NAME,
};
use simchain_common::control_api::ReorgJobRequest;
use simchain_common::require_regtest_address;
use simchain_reorg::{
    run_once, ReorgObserver, ReorgPhase, ReorgProgress, ReorgRequest, ReorgTarget, WitnessTarget,
};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

pub struct ReorgExecution {
    pub result: serde_json::Value,
    pub chain_changed: bool,
    pub aborted: bool,
}

/// The minimum durable information needed to make a possibly-invalidated
/// target node safe after an executor failure or control-plane restart.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReorgRecoveryContext {
    pub mutation_may_have_occurred: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalidated_block_hash: Option<String>,
}

pub trait ReorgExecutor: Send + Sync {
    fn execute(
        &self,
        request: &ReorgJobRequest,
        use_raw_tx_spam: bool,
        observer: &dyn ReorgObserver,
    ) -> anyhow::Result<ReorgExecution>;

    /// Verify or restore a safe converged chain for a previously interrupted
    /// request. This never resumes transaction-selection workflow; it only
    /// re-allows the old branch or extends an already-built replacement until
    /// the strict witness agrees.
    fn recover(
        &self,
        request: &ReorgJobRequest,
        context: &ReorgRecoveryContext,
        observer: &dyn ReorgObserver,
    ) -> anyhow::Result<()>;
}

pub struct RpcReorgExecutor {
    node1_url: RpcUrl,
    node2_url: RpcUrl,
    node3_url: RpcUrl,
    node2_wallet: String,
    node3_wallet: String,
    mine_address: Address,
}

impl RpcReorgExecutor {
    pub fn from_config(config: &ControlPlaneConfig) -> anyhow::Result<Self> {
        let mine_address = std::env::var("REORG_MINE_ADDRESS")
            .unwrap_or_else(|_| "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr".to_string());
        let mine_address = mine_address
            .parse::<Address<NetworkUnchecked>>()
            .map_err(|error| anyhow::anyhow!("invalid REORG_MINE_ADDRESS: {error}"))?;
        Ok(Self {
            node1_url: parse_rpc_url("NODE1_RPC_URL", config.node1_url.clone())?,
            node2_url: parse_rpc_url("NODE2_RPC_URL", config.node2_url.clone())?,
            node3_url: parse_rpc_url("NODE3_RPC_URL", config.node3_url.clone())?,
            node2_wallet: non_empty_env("NODE2_WALLET_NAME", DEFAULT_NODE2_WALLET_NAME),
            node3_wallet: non_empty_env("NODE3_WALLET_NAME", DEFAULT_NODE3_WALLET_NAME),
            mine_address: require_regtest_address(mine_address)?,
        })
    }
}

impl ReorgExecutor for RpcReorgExecutor {
    fn execute(
        &self,
        request: &ReorgJobRequest,
        use_raw_tx_spam: bool,
        observer: &dyn ReorgObserver,
    ) -> anyhow::Result<ReorgExecution> {
        let (node_name, rpc_url, wallet_name) = self.resolve_target(request)?;
        let target = ReorgTarget {
            node_name,
            rpc_url,
            mine_address: self.mine_address.clone(),
            wallet_name,
            witness: Some(WitnessTarget {
                name: "btc-simnet-node1".to_string(),
                rpc_url: self.node1_url.clone(),
                required: true,
            }),
            use_raw_tx_spam,
        };
        let library_request = ReorgRequest {
            depth: request.depth,
            empty: request.empty,
            adds_new_txs: request.adds_new_txs,
            double_spend_pct: request.double_spend_pct,
        };
        let tracking = TrackingObserver::new(observer);
        match run_once(&target, &library_request, &tracking) {
            Ok(result) => Ok(ReorgExecution {
                chain_changed: result.chain_changed,
                aborted: result.aborted,
                result: serde_json::to_value(result)?,
            }),
            Err(primary) => {
                let context = tracking.context();
                if !context.mutation_may_have_occurred {
                    return Err(primary);
                }

                tracking.observe(ReorgProgress {
                    phase: ReorgPhase::Recovering,
                    message: format!(
                        "reorg failed after history may have changed; restoring a safe chain: {primary}"
                    ),
                    data: None,
                });
                let mut attempts = 0u64;
                loop {
                    attempts += 1;
                    match self.recover(request, &context, &tracking) {
                        Ok(()) => {
                            return Err(anyhow::anyhow!(
                                "reorg failed after history mutation, but safe chain recovery completed: {primary}"
                            ));
                        }
                        Err(error) => {
                            if attempts == 1 || attempts.is_multiple_of(15) {
                                tracking.observe(ReorgProgress {
                                    phase: ReorgPhase::RecoveryPending,
                                    message: format!(
                                        "safe reorg recovery is still pending (attempt {attempts}): {error}"
                                    ),
                                    data: Some(json!({"attempt": attempts})),
                                });
                            }
                            thread::sleep(Duration::from_secs(2));
                        }
                    }
                }
            }
        }
    }

    fn recover(
        &self,
        request: &ReorgJobRequest,
        context: &ReorgRecoveryContext,
        observer: &dyn ReorgObserver,
    ) -> anyhow::Result<()> {
        if !context.mutation_may_have_occurred {
            return Ok(());
        }
        let (_, rpc_url, _) = self.resolve_target(request)?;
        let node = simchain_common::create_client(&rpc_url)?;
        let witness = simchain_common::create_client(&self.node1_url)?;
        if tips_match(&node, &witness)? {
            emit_recovered(observer, &node)?;
            return Ok(());
        }

        let invalidated_hash = context.invalidated_block_hash.as_deref().ok_or_else(|| {
            anyhow::anyhow!("cannot recover a changed reorg without the invalidated block hash")
        })?;
        node.call::<serde_json::Value>("reconsiderblock", &[json!(invalidated_hash)])?;

        // Reconsidering restores the old witness branch when it has more
        // work. If a partial replacement already has equal/more work, extend
        // it with bounded empty blocks until node1 adopts it instead.
        for extra in 0..=10u64 {
            for _ in 0..12 {
                if tips_match(&node, &witness)? {
                    emit_recovered(observer, &node)?;
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(250));
            }
            if extra == 10 {
                break;
            }
            let node_height = node.get_block_count()?;
            let witness_height = witness.get_block_count()?;
            if node_height >= witness_height {
                node.call::<serde_json::Value>(
                    "generateblock",
                    &[json!(self.mine_address.to_string()), json!([])],
                )?;
            }
        }
        anyhow::bail!("node1 did not converge during bounded reorg recovery")
    }
}

impl RpcReorgExecutor {
    fn resolve_target(
        &self,
        request: &ReorgJobRequest,
    ) -> anyhow::Result<(String, RpcUrl, String)> {
        match request.node.as_str() {
            "node2" => Ok((
                "btc-simnet-node2".to_string(),
                self.node2_url.clone(),
                self.node2_wallet.clone(),
            )),
            "node3" => Ok((
                "btc-simnet-node3".to_string(),
                self.node3_url.clone(),
                self.node3_wallet.clone(),
            )),
            _ => anyhow::bail!("reorg node must be node2 or node3"),
        }
    }
}

struct TrackingObserver<'a> {
    inner: &'a dyn ReorgObserver,
    context: Mutex<ReorgRecoveryContext>,
}

impl<'a> TrackingObserver<'a> {
    fn new(inner: &'a dyn ReorgObserver) -> Self {
        Self {
            inner,
            context: Mutex::new(ReorgRecoveryContext::default()),
        }
    }

    fn context(&self) -> ReorgRecoveryContext {
        self.context.lock().expect("reorg tracking lock").clone()
    }
}

impl ReorgObserver for TrackingObserver<'_> {
    fn observe(&self, progress: ReorgProgress) {
        if progress.phase == ReorgPhase::Invalidating {
            let mut context = self.context.lock().expect("reorg tracking lock");
            context.mutation_may_have_occurred = true;
            context.invalidated_block_hash = progress
                .data
                .as_ref()
                .and_then(|data| data.get("hash"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
        }
        self.inner.observe(progress);
    }

    fn abort_requested(&self) -> bool {
        self.inner.abort_requested()
    }
}

fn tips_match(node: &Client, witness: &Client) -> anyhow::Result<bool> {
    Ok(node.get_best_block_hash()? == witness.get_best_block_hash()?)
}

fn emit_recovered(observer: &dyn ReorgObserver, node: &Client) -> anyhow::Result<()> {
    let height = node.get_block_count()?;
    let hash = node.get_best_block_hash()?.to_string();
    observer.observe(ReorgProgress {
        phase: ReorgPhase::Recovered,
        message: format!("safe chain recovery converged at height {height}"),
        data: Some(json!({"height": height, "hash": hash})),
    });
    Ok(())
}

fn non_empty_env(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}
