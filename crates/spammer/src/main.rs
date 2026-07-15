//! Simchain transaction spammer entry point.

mod burn;
mod config;
mod control;
mod error;
mod node_wallet_spammer;
mod raw_transaction_spammer;
mod runner;
mod server;
mod wallet;

use config::SpamConfig;
use simchain_common::init_tracing;

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing("simchain_spammer=info,info");
    SpamConfig::init()?;

    runner::run()
}
