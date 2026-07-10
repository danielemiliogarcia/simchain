//! Simchain mining controller: bootstraps the chain (wallet funding and
//! coinbase maturity), then mines continuously on the two miner nodes.

mod bootstrap;
mod chain_view;
mod config;
mod mining;
mod rng;
mod runner;
mod wallet;

use config::Config;
use simchain_common::init_tracing;

fn main() -> anyhow::Result<()> {
    init_tracing("simchain_mining_controller=info,info");

    runner::run(&Config::from_env()?)
}
