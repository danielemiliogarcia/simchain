//! Environment-driven configuration: parsing and validation of every
//! setting the controller reads, collected into one [`Config`].

use anyhow::Context;
use bitcoincore_rpc::bitcoin::{address::NetworkUnchecked, Address};
use simchain_common::{env_or, require_regtest_address};
use std::time::Duration;

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

/// Everything the controller reads from the environment. Every setting has a
/// default matching docker-compose.yml, so the tool also runs standalone with
/// no environment at all.
pub struct Config {
    pub user_address: Address,
    pub mean_secs: u64,
    pub poisson: bool,
    pub interval_bounds: IntervalBounds,
    pub miner_weights: Option<MinerWeights>,
    pub configured_seed: Option<u64>,
    pub rpc_user: String,
    pub rpc_pass: String,
    pub wallet2_name: String,
    pub wallet3_name: String,
    pub node2_url: String,
    pub node3_url: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        let user_address = env_or(
            "USER_ADDRESS",
            "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr",
        );
        let user_address: Address<NetworkUnchecked> =
            user_address.parse().context("Invalid Bitcoin address")?;
        let user_address = require_regtest_address(user_address)
            .context("USER_ADDRESS must be a regtest address")?;

        let mean_secs = parse_mean_secs(&env_or("BLOCK_INTERVAL_MEAN_SECS", "15"));
        let poisson = parse_interval_mode(&env_or("BLOCK_INTERVAL_MODE", "poisson"));
        let interval_bounds = parse_interval_bounds(
            &env_or("BLOCK_INTERVAL_MIN_SECS", "10"),
            &env_or("BLOCK_INTERVAL_MAX_SECS", "20"),
        );
        if poisson {
            validate_poisson_mean(mean_secs, interval_bounds);
        }
        let miner_weights = parse_miner_weights(&env_or("MINER_WEIGHTS", ""));
        // Parse a supplied seed even when stochastic modes are disabled, so a
        // typo cannot lie dormant until a mode is enabled later.
        let configured_seed = parse_rng_seed(&env_or("MINING_RNG_SEED", ""));

        Ok(Config {
            user_address,
            mean_secs,
            poisson,
            interval_bounds,
            miner_weights,
            configured_seed,
            rpc_user: env_or("BTC_RPC_USER", "foo"),
            rpc_pass: env_or("BTC_RPC_PASS", "rpcpassword"),
            wallet2_name: env_or("NODE2_WALLET_NAME", "node2"),
            wallet3_name: env_or("NODE3_WALLET_NAME", "node3"),
            node2_url: env_or("NODE2_RPC_URL", "http://btc-simnet-node2:18443"),
            node3_url: env_or("NODE3_RPC_URL", "http://btc-simnet-node3:18443"),
        })
    }
}

fn parse_mean_secs(value: &str) -> u64 {
    let seconds: u64 = value
        .parse()
        .expect("BLOCK_INTERVAL_MEAN_SECS must be a positive integer");
    assert!(
        seconds > 0,
        "BLOCK_INTERVAL_MEAN_SECS must be a positive integer"
    );
    seconds
}

fn parse_interval_mode(value: &str) -> bool {
    match value {
        "fixed" => false,
        "poisson" => true,
        _ => panic!("BLOCK_INTERVAL_MODE must be 'fixed' or 'poisson', got '{value}'"),
    }
}

fn parse_interval_bound(key: &str, value: &str) -> Option<f64> {
    if value.trim().is_empty() {
        return None;
    }

    let seconds: f64 = value
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{key} must be a non-negative finite number"));
    assert!(
        seconds.is_finite() && seconds >= 0.0,
        "{key} must be a non-negative finite number"
    );
    assert!(
        Duration::try_from_secs_f64(seconds).is_ok(),
        "{key} is too large to represent as a duration"
    );
    Some(seconds)
}

fn parse_interval_bounds(min: &str, max: &str) -> IntervalBounds {
    let min = parse_interval_bound("BLOCK_INTERVAL_MIN_SECS", min);
    let max = parse_interval_bound("BLOCK_INTERVAL_MAX_SECS", max);
    if let Some(max) = max {
        assert!(
            max > 0.0,
            "BLOCK_INTERVAL_MAX_SECS must be greater than zero"
        );
    }
    if let (Some(min), Some(max)) = (min, max) {
        assert!(
            min <= max,
            "BLOCK_INTERVAL_MIN_SECS must not exceed BLOCK_INTERVAL_MAX_SECS"
        );
    }
    IntervalBounds { min, max }
}

// A mean outside the clamp range pins nearly every interval to a boundary --
// almost certainly a leftover bound after the mean was changed, not intent.
// Poisson mode only: fixed mode ignores the bounds, and the full-block recipes
// legitimately combine a long fixed interval with the default bounds.
fn validate_poisson_mean(mean_secs: u64, bounds: IntervalBounds) {
    let mean = mean_secs as f64;
    if let Some(min) = bounds.min {
        assert!(
            mean >= min,
            "BLOCK_INTERVAL_MEAN_SECS ({mean_secs}) is below BLOCK_INTERVAL_MIN_SECS ({min}): nearly every interval would clamp to the minimum. Raise the mean, lower the bound, or use fixed mode"
        );
    }
    if let Some(max) = bounds.max {
        assert!(
            mean <= max,
            "BLOCK_INTERVAL_MEAN_SECS ({mean_secs}) exceeds BLOCK_INTERVAL_MAX_SECS ({max}): nearly every interval would clamp to the maximum. Lower the mean, raise the bound, or use fixed mode"
        );
    }
}

fn parse_miner_weights(value: &str) -> Option<MinerWeights> {
    if value.trim().is_empty() {
        return None;
    }

    let parts: Vec<&str> = value.split(',').collect();
    assert!(
        parts.len() == 2,
        "MINER_WEIGHTS must have exactly 2 entries (node2,node3), got {}",
        parts.len()
    );
    let parse = |part: &str| {
        part.trim()
            .parse::<u64>()
            .expect("MINER_WEIGHTS must be two non-negative integers, e.g. 70,30")
    };
    let node2 = parse(parts[0]);
    let node3 = parse(parts[1]);
    let total = node2
        .checked_add(node3)
        .expect("MINER_WEIGHTS entries must not overflow u64 when added");
    assert!(total > 0, "MINER_WEIGHTS must not be 0,0");

    Some(MinerWeights {
        node2,
        node3,
        total,
    })
}

fn parse_rng_seed(value: &str) -> Option<u64> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.trim().parse().expect("MINING_RNG_SEED must be a u64"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_configuration() {
        assert_eq!(parse_mean_secs("15"), 15);
        assert!(!parse_interval_mode("fixed"));
        assert!(parse_interval_mode("poisson"));
        assert_eq!(
            parse_interval_bounds("", ""),
            IntervalBounds {
                min: None,
                max: None,
            }
        );
        assert_eq!(
            parse_interval_bounds("0.25", "30"),
            IntervalBounds {
                min: Some(0.25),
                max: Some(30.0),
            }
        );
        assert_eq!(parse_miner_weights(""), None);
        assert_eq!(parse_miner_weights("   "), None);
        assert_eq!(
            parse_miner_weights("70, 30"),
            Some(MinerWeights {
                node2: 70,
                node3: 30,
                total: 100,
            })
        );
        assert_eq!(parse_rng_seed("42"), Some(42));
        assert_eq!(parse_rng_seed(""), None);
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MEAN_SECS must be a positive integer")]
    fn rejects_zero_interval() {
        parse_mean_secs("0");
    }

    #[test]
    fn clamps_poisson_samples_to_configured_bounds() {
        let bounds = parse_interval_bounds("2.5", "10");
        assert_eq!(bounds.apply(0.1), 2.5);
        assert_eq!(bounds.apply(7.0), 7.0);
        assert_eq!(bounds.apply(20.0), 10.0);
    }

    #[test]
    fn accepts_poisson_mean_within_bounds() {
        validate_poisson_mean(15, parse_interval_bounds("10", "20"));
        validate_poisson_mean(15, parse_interval_bounds("", ""));
        validate_poisson_mean(10, parse_interval_bounds("10", "20"));
        validate_poisson_mean(20, parse_interval_bounds("10", "20"));
    }

    #[test]
    #[should_panic(expected = "exceeds BLOCK_INTERVAL_MAX_SECS")]
    fn rejects_poisson_mean_above_max() {
        validate_poisson_mean(60, parse_interval_bounds("10", "20"));
    }

    #[test]
    #[should_panic(expected = "is below BLOCK_INTERVAL_MIN_SECS")]
    fn rejects_poisson_mean_below_min() {
        validate_poisson_mean(5, parse_interval_bounds("10", "20"));
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MIN_SECS must not exceed")]
    fn rejects_reversed_interval_bounds() {
        parse_interval_bounds("10", "2");
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MAX_SECS must be greater than zero")]
    fn rejects_zero_max_interval() {
        parse_interval_bounds("", "0");
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MIN_SECS must be a non-negative finite number")]
    fn rejects_negative_min_interval() {
        parse_interval_bounds("-1", "");
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MODE must be 'fixed' or 'poisson'")]
    fn rejects_unknown_interval_mode() {
        parse_interval_mode("gaussian");
    }

    #[test]
    #[should_panic(expected = "MINER_WEIGHTS must have exactly 2 entries")]
    fn rejects_extra_miner_weight() {
        parse_miner_weights("1,2,3");
    }

    #[test]
    #[should_panic(expected = "MINER_WEIGHTS must not be 0,0")]
    fn rejects_zero_miner_weights() {
        parse_miner_weights("0,0");
    }

    #[test]
    #[should_panic(expected = "MINER_WEIGHTS entries must not overflow u64")]
    fn rejects_miner_weight_overflow() {
        parse_miner_weights("18446744073709551615,1");
    }

    #[test]
    #[should_panic(expected = "MINING_RNG_SEED must be a u64")]
    fn rejects_invalid_seed() {
        parse_rng_seed("not-a-number");
    }
}
