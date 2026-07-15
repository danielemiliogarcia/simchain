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
use std::{collections::BTreeMap, net::SocketAddr, sync::OnceLock};

static SPAM_CONFIG: OnceLock<SpamConfig> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct SpamConfig {
    pub initial_policy: SpamTuning,
    pub wallet2_name: String,
    pub wallet3_name: String,
    pub node1_url: RpcUrl,
    pub node2_url: RpcUrl,
    pub node3_url: RpcUrl,
    pub control_listen_addr: SocketAddr,
    pub internal_token: String,
}

impl SpamConfig {
    pub fn init() -> Result<&'static Self, ConfigError> {
        if let Some(config) = SPAM_CONFIG.get() {
            return Ok(config);
        }

        let spam = CommonConfig::init_with(Self::from_env())?;
        let _ = SPAM_CONFIG.set(spam);
        Ok(Self::global())
    }

    pub fn global() -> &'static Self {
        SPAM_CONFIG
            .get()
            .unwrap_or_else(|| panic!("SpamConfig::init() not called in main"))
    }

    /// Node 2 takes the odd remainder so the shares always sum to the total.
    pub fn fixed_shares(policy: &SpamTuning) -> (u64, u64) {
        (
            policy.fixed_txs_per_block.div_ceil(MINER_COUNT),
            policy.fixed_txs_per_block / MINER_COUNT,
        )
    }

    fn from_env() -> Result<Self, ConfigError> {
        let mut errors = Vec::new();
        let tuning = take(&mut errors, initial_tuning());
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
        let control_listen_addr = take(&mut errors, parse_control_listen_addr());
        let internal_token = take(
            &mut errors,
            non_empty_or("SIMCHAIN_INTERNAL_TOKEN", "simchain-internal-dev-token"),
        );

        finish(errors)?;

        let (
            Some((tuning, warnings)),
            Some(node1_url),
            Some(node2_url),
            Some(node3_url),
            Some(wallet2_name),
            Some(wallet3_name),
            Some(control_listen_addr),
            Some(internal_token),
        ) = (
            tuning,
            node1_url,
            node2_url,
            node3_url,
            wallet2_name,
            wallet3_name,
            control_listen_addr,
            internal_token,
        )
        else {
            unreachable!("SpamConfig fields must be present after validation");
        };

        for warning in warnings {
            tracing::warn!("{warning}");
        }

        Ok(Self {
            initial_policy: tuning,
            wallet2_name,
            wallet3_name,
            node1_url,
            node2_url,
            node3_url,
            control_listen_addr,
            internal_token,
        })
    }
}

fn initial_tuning() -> Result<(SpamTuning, Vec<String>), ConfigError> {
    match SpamTuning::from_source(&EnvSource) {
        Ok(tuning) => Ok(tuning),
        Err(error) if !spam_enabled(&EnvSource) => {
            // Preserve the old disabled-start guarantee: dormant malformed
            // spam values cannot prevent the resident control endpoint from
            // starting. A later typed policy update supplies validated values.
            tracing::warn!(
                "ENABLE_SPAM=false: ignored invalid dormant spam settings at boot: {error}"
            );
            let mut defaults = simchain_common::live_tuning::staged_map(&BTreeMap::new());
            defaults.insert("ENABLE_SPAM".to_string(), "false".to_string());
            SpamTuning::from_source(&defaults)
        }
        Err(error) => Err(error),
    }
}

fn parse_control_listen_addr() -> Result<SocketAddr, ConfigError> {
    let value = simchain_common::config::string_or("SPAM_CONTROL_LISTEN_ADDR", "0.0.0.0:9082");
    value.parse().map_err(|error| {
        ConfigError::invalid(
            "SPAM_CONTROL_LISTEN_ADDR",
            value,
            format!("expected IP:port ({error})"),
        )
    })
}
