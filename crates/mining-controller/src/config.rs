//! Environment-driven configuration: parsing and validation of every setting
//! the controller reads, collected into one global [`MiningConfig`].
//!
//! The live-retunable subset (interval mode/mean/bounds, miner weights, RNG
//! seed) is parsed and validated by `simchain_common::live_tuning`, the same
//! module the panel uses, so a configuration the panel accepts is exactly a
//! configuration this binary accepts on restart.

use bitcoincore_rpc::bitcoin::{address::NetworkUnchecked, Address};
use simchain_common::config::{
    finish, parse_rpc_url_or, string_or, take, CommonConfig, ConfigError, RpcUrl,
    DEFAULT_NODE2_RPC_URL, DEFAULT_NODE2_WALLET_NAME, DEFAULT_NODE3_RPC_URL,
    DEFAULT_NODE3_WALLET_NAME,
};
use simchain_common::live_tuning::{EnvSource, MiningTuning};
use simchain_common::require_regtest_address;
use std::net::SocketAddr;
use std::sync::OnceLock;

static MINING_CONFIG: OnceLock<MiningConfig> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct MiningConfig {
    pub user_address: Address,
    pub initial_policy: MiningTuning,
    pub node2_url: RpcUrl,
    pub node3_url: RpcUrl,
    pub wallet2_name: String,
    pub wallet3_name: String,
    pub control_listen_addr: SocketAddr,
    pub internal_token: String,
}

impl MiningConfig {
    pub fn init() -> Result<&'static Self, ConfigError> {
        if let Some(config) = MINING_CONFIG.get() {
            return Ok(config);
        }

        let mining = CommonConfig::init_with(Self::from_env())?;
        let _ = MINING_CONFIG.set(mining);
        Ok(Self::global())
    }

    pub fn global() -> &'static Self {
        MINING_CONFIG
            .get()
            .unwrap_or_else(|| panic!("MiningConfig::init() not called in main"))
    }

    fn from_env() -> Result<Self, ConfigError> {
        let mut errors = Vec::new();
        let user_address = take(&mut errors, parse_user_address());
        let tuning = take(&mut errors, MiningTuning::from_source(&EnvSource));
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
            simchain_common::config::non_empty_or("NODE2_WALLET_NAME", DEFAULT_NODE2_WALLET_NAME),
        );
        let wallet3_name = take(
            &mut errors,
            simchain_common::config::non_empty_or("NODE3_WALLET_NAME", DEFAULT_NODE3_WALLET_NAME),
        );
        let control_listen_addr = take(&mut errors, parse_control_listen_addr());
        let internal_token = take(
            &mut errors,
            simchain_common::config::non_empty_or(
                "SIMCHAIN_INTERNAL_TOKEN",
                "simchain-internal-dev-token",
            ),
        );

        finish(errors)?;

        let (
            Some(user_address),
            Some(tuning),
            Some(node2_url),
            Some(node3_url),
            Some(wallet2_name),
            Some(wallet3_name),
            Some(control_listen_addr),
            Some(internal_token),
        ) = (
            user_address,
            tuning,
            node2_url,
            node3_url,
            wallet2_name,
            wallet3_name,
            control_listen_addr,
            internal_token,
        )
        else {
            unreachable!("MiningConfig fields must be present after validation");
        };

        Ok(Self {
            user_address,
            initial_policy: tuning,
            node2_url,
            node3_url,
            wallet2_name,
            wallet3_name,
            control_listen_addr,
            internal_token,
        })
    }
}

fn parse_control_listen_addr() -> Result<SocketAddr, ConfigError> {
    let value = string_or("MINING_CONTROL_LISTEN_ADDR", "0.0.0.0:9081");
    value.parse().map_err(|error| {
        ConfigError::invalid(
            "MINING_CONTROL_LISTEN_ADDR",
            value,
            format!("expected IP:port ({error})"),
        )
    })
}

fn parse_user_address() -> Result<Address, ConfigError> {
    let value = string_or(
        "USER_ADDRESS",
        "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr",
    );
    let parsed = value
        .parse::<Address<NetworkUnchecked>>()
        .map_err(|error| ConfigError::invalid("USER_ADDRESS", value.clone(), error.to_string()))?;
    require_regtest_address(parsed)
        .map_err(|error| ConfigError::invalid("USER_ADDRESS", value, error.to_string()))
}
