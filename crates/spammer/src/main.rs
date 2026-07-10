//! Simchain transaction spammer entry point.

mod burn;
mod config;
mod error;
mod node_wallet_spammer;
mod raw_transaction_spammer;
mod runner;
mod wallet;

use config::Config;
use simchain_common::init_tracing;

fn main() -> anyhow::Result<()> {
    init_tracing("simchain_spammer=info,info");

    runner::run(Config::from_env().as_ref())
}
