//! Simchain reorg simulator entry point.

mod config;
mod runner;

use config::ReorgConfig;
use simchain_common::init_tracing;

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing("simchain_reorg=info,info");
    ReorgConfig::init()?;

    runner::run()
}
