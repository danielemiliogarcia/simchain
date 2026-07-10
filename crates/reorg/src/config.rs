//! Environment and CLI configuration for the reorg simulator.

use bitcoincore_rpc::bitcoin::{address::NetworkUnchecked, Address};
use simchain_common::config::{
    finish, non_empty_or, parse_or, parse_rpc_url, string_or, take, CommonConfig, ConfigError,
    RpcUrl, DEFAULT_NODE3_WALLET_NAME,
};
use simchain_common::require_regtest_address;
use std::{env, sync::OnceLock};

static REORG_CONFIG: OnceLock<ReorgConfig> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReorgMode {
    Once,
    Auto,
}

#[derive(Clone, Debug)]
pub struct WitnessConfig {
    pub name: String,
    pub rpc_url: RpcUrl,
}

#[derive(Clone, Debug)]
pub struct ReorgConfig {
    pub node_name: String,
    pub rpc_url: RpcUrl,
    pub mode: ReorgMode,
    pub every: u64,
    pub adds_new_txs: u64,
    pub depth: u64,
    pub empty_mode: bool,
    pub mine_address: Address,
    pub witness: Option<WitnessConfig>,
    pub wallet_name: String,
}

impl ReorgConfig {
    pub fn init() -> Result<&'static Self, ConfigError> {
        if let Some(config) = REORG_CONFIG.get() {
            return Ok(config);
        }

        let reorg = CommonConfig::init_with(Self::from_env_and_args())?;
        let _ = REORG_CONFIG.set(reorg);
        Ok(Self::global())
    }

    pub fn global() -> &'static Self {
        REORG_CONFIG
            .get()
            .unwrap_or_else(|| panic!("ReorgConfig::init() not called in main"))
    }

    fn from_env_and_args() -> Result<Self, ConfigError> {
        let cli_args: Vec<String> = env::args().skip(1).collect();
        let empty_mode = cli_args
            .iter()
            .any(|arg| arg == "empty" || arg == "--empty");

        let mut errors = Vec::new();
        let node_name = take(&mut errors, non_empty_or("REORG_NODE", "btc-simnet-node3"));
        let rpc_port = take(&mut errors, parse_rpc_port());
        let mode = take(&mut errors, parse_reorg_mode());
        let every = take(
            &mut errors,
            parse_or::<u64>("AUTO_REORG_EVERY_BLOCKS", "20"),
        );
        let adds_new_txs = take(&mut errors, parse_or::<u64>("REORG_ADDS_NEW_TXS", "5"));
        let depth = take(&mut errors, parse_depth(&cli_args));
        let mine_address = take(&mut errors, parse_mine_address());
        let witness_name = take(
            &mut errors,
            non_empty_or("REORG_WITNESS_NODE", "btc-simnet-node1"),
        );
        let wallet_name = take(
            &mut errors,
            non_empty_or("REORG_WALLET_NAME", DEFAULT_NODE3_WALLET_NAME),
        );

        let rpc_url = match (&node_name, rpc_port) {
            (Some(node_name), Some(rpc_port)) => take(
                &mut errors,
                parse_rpc_url("REORG_NODE", format!("http://{node_name}:{rpc_port}")),
            ),
            _ => None,
        };

        let witness = match (&witness_name, &node_name, rpc_port) {
            (Some(witness_name), Some(node_name), Some(rpc_port))
                if witness_name != "none" && witness_name != node_name =>
            {
                let rpc_url = take(
                    &mut errors,
                    parse_rpc_url(
                        "REORG_WITNESS_NODE",
                        format!("http://{witness_name}:{rpc_port}"),
                    ),
                );
                Some(rpc_url.map(|rpc_url| WitnessConfig {
                    name: witness_name.clone(),
                    rpc_url,
                }))
            }
            _ => Some(None),
        };

        if let (Some(mode), Some(every), Some(depth)) = (mode, every, depth) {
            if mode == ReorgMode::Auto && every <= depth {
                errors.push(ConfigError::out_of_range(
                    "AUTO_REORG_EVERY_BLOCKS",
                    every.to_string(),
                    format!("must be greater than REORG_DEPTH ({depth}) in auto mode"),
                ));
            }
        }

        finish(errors)?;

        let (
            Some(node_name),
            Some(rpc_url),
            Some(mode),
            Some(every),
            Some(adds_new_txs),
            Some(depth),
            Some(mine_address),
            Some(witness),
            Some(wallet_name),
        ) = (
            node_name,
            rpc_url,
            mode,
            every,
            adds_new_txs,
            depth,
            mine_address,
            witness,
            wallet_name,
        )
        else {
            unreachable!("ReorgConfig fields must be present after validation");
        };

        Ok(Self {
            node_name,
            rpc_url,
            mode,
            every,
            adds_new_txs,
            depth,
            empty_mode,
            mine_address,
            witness,
            wallet_name,
        })
    }
}

fn parse_reorg_mode() -> Result<ReorgMode, ConfigError> {
    let value = string_or("REORG_MODE", "once");
    match value.trim() {
        "once" => Ok(ReorgMode::Once),
        "auto" => Ok(ReorgMode::Auto),
        _ => Err(ConfigError::invalid(
            "REORG_MODE",
            value,
            "expected one of: once, auto",
        )),
    }
}

fn parse_depth(cli_args: &[String]) -> Result<u64, ConfigError> {
    let depth = cli_args
        .iter()
        .find_map(|arg| arg.parse::<u64>().ok())
        .map_or_else(|| parse_or::<u64>("REORG_DEPTH", "3"), Ok)?;
    if depth == 0 {
        return Err(ConfigError::out_of_range(
            "REORG_DEPTH",
            depth.to_string(),
            "must be at least 1",
        ));
    }
    Ok(depth)
}

fn parse_rpc_port() -> Result<u16, ConfigError> {
    let port = parse_or::<u16>("REORG_NODE_RPC_PORT", "18443")?;
    if port == 0 {
        return Err(ConfigError::out_of_range(
            "REORG_NODE_RPC_PORT",
            port.to_string(),
            "must be greater than zero",
        ));
    }
    Ok(port)
}

fn parse_mine_address() -> Result<Address, ConfigError> {
    let value = string_or(
        "REORG_MINE_ADDRESS",
        "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr",
    );
    let parsed = value
        .parse::<Address<NetworkUnchecked>>()
        .map_err(|error| {
            ConfigError::invalid("REORG_MINE_ADDRESS", value.clone(), error.to_string())
        })?;
    require_regtest_address(parsed)
        .map_err(|error| ConfigError::invalid("REORG_MINE_ADDRESS", value, error.to_string()))
}
