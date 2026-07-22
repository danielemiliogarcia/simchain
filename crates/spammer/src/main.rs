//! Simchain transaction spammer entry point.

use simchain_common::init_tracing;
use simchain_spammer::config::SpamConfig;
use simchain_spammer::runner;

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing("simchain_spammer=info,info");
    SpamConfig::init()?;

    runner::run()
}
