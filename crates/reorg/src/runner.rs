//! Top-level reorg operation: connect to nodes and select once or automatic
//! operation. The chain-rewrite details live in the `simchain_reorg` library.

use crate::config::{ReorgConfig, ReorgMode};
use anyhow::Context;
use bitcoincore_rpc::{Client, RpcApi};
use simchain_common::{create_client, wait_for_rpc};
use simchain_reorg::{run_once, NoopObserver, ReorgRequest, ReorgTarget, WitnessTarget};
use std::{thread, time::Duration};

pub fn run() -> anyhow::Result<()> {
    let config = ReorgConfig::global();
    let node = create_client(&config.rpc_url).context("build reorg node client")?;
    wait_for_rpc(&node, &config.node_name, Duration::from_secs(1));

    let target = target(config);
    let request = request(config);
    match config.mode {
        ReorgMode::Once => run_once(&target, &request, &NoopObserver)
            .context("reorg failed")
            .map(|_| ()),
        ReorgMode::Auto => run_automatically(config, &node, &target, &request),
    }
}

fn run_automatically(
    config: &ReorgConfig,
    node: &Client,
    target: &ReorgTarget,
    request: &ReorgRequest,
) -> anyhow::Result<()> {
    let mut last = node.get_block_count().context("get_block_count failed")?;
    tracing::info!(
        "Auto-reorg mode: every {} blocks, reorg the last {} (current height {last})",
        config.every,
        config.depth
    );
    loop {
        match node.get_block_count() {
            Ok(tip) if tip >= last + config.every => {
                if let Err(error) = run_once(target, request, &NoopObserver) {
                    tracing::error!("Reorg failed: {error}");
                }
                last = node.get_block_count().unwrap_or(tip);
            }
            Ok(_) => {}
            Err(error) => tracing::warn!("RPC error while polling height: {error}"),
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn request(config: &ReorgConfig) -> ReorgRequest {
    ReorgRequest {
        depth: config.depth,
        empty: config.empty_mode,
        adds_new_txs: config.adds_new_txs,
        double_spend_pct: config.double_spend_pct,
    }
}

fn target(config: &ReorgConfig) -> ReorgTarget {
    ReorgTarget {
        node_name: config.node_name.clone(),
        rpc_url: config.rpc_url.clone(),
        mine_address: config.mine_address.clone(),
        wallet_name: config.wallet_name.clone(),
        witness: config.witness.as_ref().map(|witness| WitnessTarget {
            name: witness.name.clone(),
            rpc_url: witness.rpc_url.clone(),
            required: false,
        }),
        use_raw_tx_spam: config.use_raw_tx_spam,
    }
}
