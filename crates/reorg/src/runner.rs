//! Top-level reorg operation: connect to nodes and select once or automatic
//! operation. The chain-rewrite details live in [`crate::reorg`].

use crate::{
    config::{ReorgConfig, ReorgMode},
    reorg,
};
use anyhow::Context;
use bitcoincore_rpc::{Client, RpcApi};
use simchain_common::{create_client, wait_for_rpc};
use std::{thread, time::Duration};

pub fn run() -> anyhow::Result<()> {
    let config = ReorgConfig::global();
    let node = create_client(&config.rpc_url).context("build reorg node client")?;
    wait_for_rpc(&node, &config.node_name, Duration::from_secs(1));

    // Witness node: another node polled after the reorg to confirm the whole
    // network adopted the new chain (node1 never mines, ideal witness).
    // REORG_WITNESS_NODE=none disables the check.
    let witness_client = match config.witness.as_ref() {
        Some(witness) => {
            Some(create_client(&witness.rpc_url).context("build witness node client")?)
        }
        None => None,
    };
    let witness: Option<(&Client, &str)> = witness_client
        .as_ref()
        .zip(config.witness.as_ref())
        .map(|(client, witness)| (client, witness.name.as_str()));

    match config.mode {
        ReorgMode::Once => reorg::run(&node, witness).context("reorg failed"),
        ReorgMode::Auto => run_automatically(&node, witness),
    }
}

fn run_automatically(node: &Client, witness: Option<(&Client, &str)>) -> anyhow::Result<()> {
    let config = ReorgConfig::global();
    let mut last = node.get_block_count().context("get_block_count failed")?;
    tracing::info!(
        "Auto-reorg mode: every {} blocks, reorg the last {} (current height {last})",
        config.every,
        config.depth
    );
    loop {
        match node.get_block_count() {
            Ok(tip) if tip >= last + config.every => {
                if let Err(error) = reorg::run(node, witness) {
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
