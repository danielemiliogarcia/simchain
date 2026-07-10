//! Simchain reorg simulator entry point.

mod chain;
mod config;
mod reorg;
mod runner;
mod wallet;

use config::Config;
use simchain_common::init_tracing;

fn main() -> anyhow::Result<()> {
    init_tracing("simchain_reorg=info,info");

    runner::run(&Config::load()?)
}
