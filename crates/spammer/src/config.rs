//! Environment-driven configuration for the spammer.
//!
//! The live-retunable subset (engine selection, fee floor, fill targets,
//! fanout, RBF traffic) is parsed and validated by
//! `simchain_common::live_tuning`, the same module the panel uses, so a
//! configuration the panel accepts is exactly a configuration this binary
//! accepts on restart.

use crate::burn::MINER_COUNT;
use simchain_common::config::{
    finish, non_empty_or, parse_rpc_url_or, take, CommonConfig, ConfigError, RpcUrl,
    DEFAULT_NODE1_RPC_URL, DEFAULT_NODE2_RPC_URL, DEFAULT_NODE2_WALLET_NAME, DEFAULT_NODE3_RPC_URL,
    DEFAULT_NODE3_WALLET_NAME,
};
use simchain_common::live_tuning::{spam_enabled, EnvSource, SpamTuning};
use std::sync::OnceLock;

static SPAM_CONFIG: OnceLock<Option<SpamConfig>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct SpamConfig {
    pub use_raw: bool,
    pub fixed_txs_per_block: u64,
    pub fanout_utxos: u64,
    pub sendmany_outputs: u64,
    pub data_min_bytes: u64,
    pub data_max_bytes: u64,
    pub small_txs_per_block: u64,
    pub fill_block_ratio: f64,
    pub floor_pool_txs: u64,
    pub fanout_auto: bool,
    pub enable_replaces: bool,
    pub replaces_per_miner: u64,
    pub fallback_fee: f64,
    pub fee_rate_sat_vb: f64,
    pub wallet2_name: String,
    pub wallet3_name: String,
    pub node1_url: RpcUrl,
    pub node2_url: RpcUrl,
    pub node3_url: RpcUrl,
}

impl SpamConfig {
    pub fn init() -> Result<Option<&'static Self>, ConfigError> {
        if let Some(config) = SPAM_CONFIG.get() {
            return Ok(config.as_ref());
        }
        if !spam_enabled(&EnvSource) {
            let _ = SPAM_CONFIG.set(None);
            return Ok(None);
        }

        let spam = CommonConfig::init_with(Self::from_enabled_env())?;
        let _ = SPAM_CONFIG.set(Some(spam));
        Ok(SPAM_CONFIG.get().and_then(Option::as_ref))
    }

    pub fn global() -> &'static Self {
        SPAM_CONFIG
            .get()
            .and_then(Option::as_ref)
            .unwrap_or_else(|| panic!("SpamConfig::init() not called in main or spam is disabled"))
    }

    pub fn is_enabled() -> bool {
        SPAM_CONFIG.get().and_then(Option::as_ref).is_some()
    }

    /// Node 2 takes the odd remainder so the shares always sum to the total.
    pub fn fixed_shares(&self) -> (u64, u64) {
        (
            self.fixed_txs_per_block.div_ceil(MINER_COUNT),
            self.fixed_txs_per_block / MINER_COUNT,
        )
    }

    fn from_enabled_env() -> Result<Self, ConfigError> {
        let mut errors = Vec::new();
        let tuning = take(&mut errors, SpamTuning::from_source(&EnvSource));
        let node1_url = take(
            &mut errors,
            parse_rpc_url_or("NODE1_RPC_URL", DEFAULT_NODE1_RPC_URL),
        );
        let node2_url = take(
            &mut errors,
            parse_rpc_url_or("NODE2_RPC_URL", DEFAULT_NODE2_RPC_URL),
        );
        let node3_url = take(
            &mut errors,
            parse_rpc_url_or("NODE3_RPC_URL", DEFAULT_NODE3_RPC_URL),
        );
        let wallet2_name = take(
            &mut errors,
            non_empty_or("NODE2_WALLET_NAME", DEFAULT_NODE2_WALLET_NAME),
        );
        let wallet3_name = take(
            &mut errors,
            non_empty_or("NODE3_WALLET_NAME", DEFAULT_NODE3_WALLET_NAME),
        );

        finish(errors)?;

        let (
            Some((tuning, warnings)),
            Some(node1_url),
            Some(node2_url),
            Some(node3_url),
            Some(wallet2_name),
            Some(wallet3_name),
        ) = (
            tuning,
            node1_url,
            node2_url,
            node3_url,
            wallet2_name,
            wallet3_name,
        )
        else {
            unreachable!("SpamConfig fields must be present after validation");
        };

        for warning in warnings {
            tracing::warn!("{warning}");
        }

        Ok(Self {
            use_raw: tuning.use_raw,
            fixed_txs_per_block: tuning.fixed_txs_per_block,
            fanout_utxos: tuning.fanout_utxos,
            sendmany_outputs: tuning.sendmany_outputs,
            data_min_bytes: tuning.effective_data_min_bytes(),
            data_max_bytes: tuning.data_max_bytes,
            small_txs_per_block: tuning.small_txs_per_block,
            fill_block_ratio: tuning.fill_block_ratio,
            floor_pool_txs: tuning.floor_pool_txs,
            fanout_auto: tuning.fanout_auto,
            enable_replaces: tuning.enable_replaces,
            replaces_per_miner: tuning.replaces_per_miner,
            fallback_fee: tuning.fallback_fee,
            fee_rate_sat_vb: tuning.fee_rate_sat_vb(),
            wallet2_name,
            wallet3_name,
            node1_url,
            node2_url,
            node3_url,
        })
    }
}
