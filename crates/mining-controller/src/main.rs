//! Simchain mining controller: bootstraps the chain (wallet funding and
//! coinbase maturity), then mines continuously on the two miner nodes.

mod bootstrap;
mod chain_view;
mod config;
mod control;
mod mining;
mod rng;
mod runner;
mod server;
mod wallet;

use config::MiningConfig;
use simchain_common::init_tracing;

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing("simchain_mining_controller=info,info");
    MiningConfig::init()?;

    runner::run()
}
