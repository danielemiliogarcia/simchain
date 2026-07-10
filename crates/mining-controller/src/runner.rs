//! Controller startup: connect to the miner nodes, prepare wallets, bootstrap
//! the chain, and hand control to the continuous mining loop.

use crate::{
    bootstrap,
    config::Config,
    mining,
    rng::{entropy_seed, Rng},
    wallet::setup_wallet,
};
use anyhow::Context;
use simchain_common::{create_client, wait_for_rpc};
use std::time::Duration;

pub fn run(config: &Config) -> anyhow::Result<()> {
    let seed = config.configured_seed.unwrap_or_else(entropy_seed);
    let rng = Rng::new(seed);

    let node2 = create_client(&config.node2_url, &config.rpc_user, &config.rpc_pass)
        .context("build node2 client")?;
    let node3 = create_client(&config.node3_url, &config.rpc_user, &config.rpc_pass)
        .context("build node3 client")?;

    tracing::info!("Waiting for nodes to be ready");
    wait_for_rpc(&node2, "node2", Duration::from_millis(200));
    wait_for_rpc(&node3, "node3", Duration::from_millis(200));

    let (_wallet2, addr2) = setup_wallet(
        &config.node2_url,
        &config.rpc_user,
        &config.rpc_pass,
        &node2,
        &config.wallet2_name,
    )?;
    let (_wallet3, addr3) = setup_wallet(
        &config.node3_url,
        &config.rpc_user,
        &config.rpc_pass,
        &node3,
        &config.wallet3_name,
    )?;

    bootstrap::run(&node2, &node3, &addr2, &addr3, &config.user_address)?;

    mining::run(config, seed, rng, &node2, &node3, &addr2, &addr3)
}
