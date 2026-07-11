//! Simchain reorg simulator entry point.

mod chain;
mod config;
mod double_spend;
mod reorg;
mod runner;
mod wallet;

use config::ReorgConfig;
use simchain_common::init_tracing;

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing("simchain_reorg=info,info");
    ReorgConfig::init()?;

    runner::run()
}
