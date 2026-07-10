//! Top-level reorg operation: connect to nodes and select once or automatic
//! operation. The chain-rewrite details live in [`crate::reorg`].

use crate::{config::Config, reorg};
use anyhow::{anyhow, Context};
use bitcoincore_rpc::{Client, RpcApi};
use simchain_common::{create_client, wait_for_rpc};
use std::{thread, time::Duration};

pub fn run(config: &Config) -> anyhow::Result<()> {
    let node = create_client(&config.rpc_url, &config.rpc_user, &config.rpc_pass)
        .context("build reorg node client")?;
    wait_for_rpc(&node, &config.node_name, Duration::from_secs(1));

    // Witness node: another node polled after the reorg to confirm the whole
    // network adopted the new chain (node1 never mines, ideal witness).
    // REORG_WITNESS_NODE=none disables the check.
    let witness_client = if config.witness_name == "none" || config.witness_name == config.node_name
    {
        None
    } else {
        Some(
            create_client(
                &format!("http://{}:{}", config.witness_name, config.rpc_port),
                &config.rpc_user,
                &config.rpc_pass,
            )
            .context("build witness node client")?,
        )
    };
    let witness: Option<(&Client, &str)> = witness_client
        .as_ref()
        .map(|client| (client, config.witness_name.as_str()));

    match config.mode.as_str() {
        "once" => reorg::run(&node, config, witness).context("reorg failed"),
        "auto" => run_automatically(&node, config, witness),
        other => Err(anyhow!(
            "Unknown REORG_MODE '{other}' (expected: once | auto)"
        )),
    }
}

fn run_automatically(
    node: &Client,
    config: &Config,
    witness: Option<(&Client, &str)>,
) -> anyhow::Result<()> {
    if config.every <= config.depth {
        return Err(anyhow!(
            "AUTO_REORG_EVERY_BLOCKS ({}) must be greater than REORG_DEPTH ({})",
            config.every,
            config.depth
        ));
    }
    let mut last = node.get_block_count().context("get_block_count failed")?;
    tracing::info!(
        "Auto-reorg mode: every {} blocks, reorg the last {} (current height {last})",
        config.every,
        config.depth
    );
    loop {
        match node.get_block_count() {
            Ok(tip) if tip >= last + config.every => {
                if let Err(error) = reorg::run(node, config, witness) {
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
