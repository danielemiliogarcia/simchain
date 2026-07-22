//! Simchain spammer: resident worker binary plus a library surface so other
//! services (the control plane's scenario jobs) can drive the raw engine
//! without going through the miner node wallets.

pub mod burn;
pub mod config;
pub mod control;
pub mod error;
pub mod node_wallet_spammer;
pub mod raw_transaction_spammer;
pub mod runner;
pub mod server;
pub mod wallet;
