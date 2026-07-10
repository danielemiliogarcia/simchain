//! Environment-driven configuration: parsing and validation of every setting
//! the controller reads, collected into one global [`MiningConfig`].

use bitcoincore_rpc::bitcoin::{address::NetworkUnchecked, Address};
use simchain_common::config::{
    finish, parse_optional, parse_or, parse_rpc_url_or, string_or, take, CommonConfig, ConfigError,
    RpcUrl, DEFAULT_NODE2_RPC_URL, DEFAULT_NODE2_WALLET_NAME, DEFAULT_NODE3_RPC_URL,
    DEFAULT_NODE3_WALLET_NAME,
};
use simchain_common::require_regtest_address;
use std::{sync::OnceLock, time::Duration};

static MINING_CONFIG: OnceLock<MiningConfig> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockIntervalMode {
    Fixed,
    Poisson,
}

impl BlockIntervalMode {
    pub fn is_poisson(self) -> bool {
        matches!(self, BlockIntervalMode::Poisson)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MinerWeights {
    pub node2: u64,
    pub node3: u64,
    pub total: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntervalBounds {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl IntervalBounds {
    pub fn apply(self, sample: f64) -> f64 {
        let above_min = self.min.map_or(sample, |min| sample.max(min));
        self.max.map_or(above_min, |max| above_min.min(max))
    }

    pub fn description(self) -> String {
        match (self.min, self.max) {
            (None, None) => "unbounded".to_string(),
            (Some(min), None) => format!("[{min}s, unbounded)"),
            (None, Some(max)) => format!("[0s, {max}s]"),
            (Some(min), Some(max)) => format!("[{min}s, {max}s]"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MiningConfig {
    pub user_address: Address,
    pub mean_secs: u64,
    pub interval_mode: BlockIntervalMode,
    pub interval_bounds: IntervalBounds,
    pub miner_weights: Option<MinerWeights>,
    pub configured_seed: Option<u64>,
    pub node2_url: RpcUrl,
    pub node3_url: RpcUrl,
    pub wallet2_name: String,
    pub wallet3_name: String,
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
        let mean_secs = take(
            &mut errors,
            parse_positive_u64("BLOCK_INTERVAL_MEAN_SECS", "15"),
        );
        let interval_mode = take(&mut errors, parse_interval_mode());
        let interval_bounds = take(&mut errors, parse_interval_bounds());
        let miner_weights = take(&mut errors, parse_miner_weights());
        let configured_seed = take(&mut errors, parse_optional::<u64>("MINING_RNG_SEED"));
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

        if let (Some(mean_secs), Some(interval_mode), Some(interval_bounds)) =
            (mean_secs, interval_mode, interval_bounds)
        {
            if interval_mode.is_poisson() {
                validate_poisson_mean(&mut errors, mean_secs, interval_bounds);
            }
        }

        finish(errors)?;

        let (
            Some(user_address),
            Some(mean_secs),
            Some(interval_mode),
            Some(interval_bounds),
            Some(miner_weights),
            Some(configured_seed),
            Some(node2_url),
            Some(node3_url),
            Some(wallet2_name),
            Some(wallet3_name),
        ) = (
            user_address,
            mean_secs,
            interval_mode,
            interval_bounds,
            miner_weights,
            configured_seed,
            node2_url,
            node3_url,
            wallet2_name,
            wallet3_name,
        )
        else {
            unreachable!("MiningConfig fields must be present after validation");
        };

        Ok(Self {
            user_address,
            mean_secs,
            interval_mode,
            interval_bounds,
            miner_weights,
            configured_seed,
            node2_url,
            node3_url,
            wallet2_name,
            wallet3_name,
        })
    }
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

fn parse_positive_u64(key: &'static str, default: &'static str) -> Result<u64, ConfigError> {
    let value = parse_or::<u64>(key, default)?;
    if value == 0 {
        return Err(ConfigError::out_of_range(
            key,
            value.to_string(),
            "must be a positive integer",
        ));
    }
    Ok(value)
}

fn parse_interval_mode() -> Result<BlockIntervalMode, ConfigError> {
    let value = string_or("BLOCK_INTERVAL_MODE", "poisson");
    match value.trim() {
        "fixed" => Ok(BlockIntervalMode::Fixed),
        "poisson" => Ok(BlockIntervalMode::Poisson),
        _ => Err(ConfigError::invalid(
            "BLOCK_INTERVAL_MODE",
            value,
            "expected one of: fixed, poisson",
        )),
    }
}

fn parse_interval_bound(key: &'static str) -> Result<Option<f64>, ConfigError> {
    let seconds = parse_optional::<f64>(key)?;
    let Some(seconds) = seconds else {
        return Ok(None);
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(ConfigError::out_of_range(
            key,
            seconds.to_string(),
            "must be a non-negative finite number",
        ));
    }
    if Duration::try_from_secs_f64(seconds).is_err() {
        return Err(ConfigError::out_of_range(
            key,
            seconds.to_string(),
            "is too large to represent as a duration",
        ));
    }
    Ok(Some(seconds))
}

fn parse_interval_bounds() -> Result<IntervalBounds, ConfigError> {
    let mut errors = Vec::new();
    let min = take(&mut errors, parse_interval_bound("BLOCK_INTERVAL_MIN_SECS"));
    let max = take(&mut errors, parse_interval_bound("BLOCK_INTERVAL_MAX_SECS"));

    if let Some(Some(max)) = max {
        if max <= 0.0 {
            errors.push(ConfigError::out_of_range(
                "BLOCK_INTERVAL_MAX_SECS",
                max.to_string(),
                "must be greater than zero",
            ));
        }
    }
    if let (Some(Some(min)), Some(Some(max))) = (min, max) {
        if min > max {
            errors.push(ConfigError::out_of_range(
                "BLOCK_INTERVAL_MIN_SECS",
                min.to_string(),
                "must not exceed BLOCK_INTERVAL_MAX_SECS",
            ));
        }
    }

    finish(errors)?;

    let (Some(min), Some(max)) = (min, max) else {
        unreachable!("Interval bounds must be present after validation");
    };

    Ok(IntervalBounds { min, max })
}

fn validate_poisson_mean(errors: &mut Vec<ConfigError>, mean_secs: u64, bounds: IntervalBounds) {
    let mean = mean_secs as f64;
    if let Some(min) = bounds.min {
        if mean < min {
            errors.push(ConfigError::out_of_range(
                "BLOCK_INTERVAL_MEAN_SECS",
                mean_secs.to_string(),
                format!(
                    "is below BLOCK_INTERVAL_MIN_SECS ({min}): nearly every interval would clamp to the minimum"
                ),
            ));
        }
    }
    if let Some(max) = bounds.max {
        if mean > max {
            errors.push(ConfigError::out_of_range(
                "BLOCK_INTERVAL_MEAN_SECS",
                mean_secs.to_string(),
                format!(
                    "exceeds BLOCK_INTERVAL_MAX_SECS ({max}): nearly every interval would clamp to the maximum"
                ),
            ));
        }
    }
}

fn parse_miner_weights() -> Result<Option<MinerWeights>, ConfigError> {
    let value = string_or("MINER_WEIGHTS", "");
    if value.trim().is_empty() {
        return Ok(None);
    }

    let parts: Vec<_> = value.split(',').map(str::trim).collect();
    if parts.len() != 2 {
        return Err(ConfigError::invalid(
            "MINER_WEIGHTS",
            value.clone(),
            format!(
                "expected exactly 2 entries (node2,node3), got {}",
                parts.len()
            ),
        ));
    }

    let node2 = parse_or_weight("MINER_WEIGHTS", parts[0], &value)?;
    let node3 = parse_or_weight("MINER_WEIGHTS", parts[1], &value)?;
    let Some(total) = node2.checked_add(node3) else {
        return Err(ConfigError::out_of_range(
            "MINER_WEIGHTS",
            value,
            "entries must not overflow u64 when added",
        ));
    };
    if total == 0 {
        return Err(ConfigError::out_of_range(
            "MINER_WEIGHTS",
            value,
            "must not be 0,0",
        ));
    }

    Ok(Some(MinerWeights {
        node2,
        node3,
        total,
    }))
}

fn parse_or_weight(key: &'static str, part: &str, full_value: &str) -> Result<u64, ConfigError> {
    part.parse::<u64>().map_err(|error| {
        ConfigError::invalid(
            key,
            full_value.to_string(),
            format!("expected two non-negative integers, e.g. 70,30 ({error})"),
        )
    })
}
