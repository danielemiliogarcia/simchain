//! Environment-driven configuration for the spammer.

use crate::burn::MINER_COUNT;
use simchain_common::config::{
    finish, non_empty_or, parse_bool_or, parse_or, parse_rpc_url_or, parse_value, read, string_or,
    take, CommonConfig, ConfigError, RpcUrl, DEFAULT_NODE1_RPC_URL, DEFAULT_NODE2_RPC_URL,
    DEFAULT_NODE2_WALLET_NAME, DEFAULT_NODE3_RPC_URL, DEFAULT_NODE3_WALLET_NAME,
};
use std::sync::OnceLock;

/// Largest OP_RETURN payload that keeps the resulting transaction below
/// Bitcoin Core's standard transaction-size limit.
const MAX_DATA_BYTES: u64 = 98_000;

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
        if !Self::enabled_from_env() {
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

    fn enabled_from_env() -> bool {
        string_or("ENABLE_SPAM", "true") == "true"
    }

    fn from_enabled_env() -> Result<Self, ConfigError> {
        let mut errors = Vec::new();
        let use_raw = take(&mut errors, parse_bool_or("USE_RAW_TX_SPAM", "true"));
        let fixed_txs_per_block = take(&mut errors, parse_fixed_txs_per_block());
        let fanout_utxos = take(&mut errors, parse_or::<u64>("SPAM_FANOUT_UTXOS", "50"));
        let sendmany_outputs = take(&mut errors, parse_or::<u64>("SPAM_SENDMANY_OUTPUTS", "0"));
        let data_max_bytes = take(&mut errors, parse_data_max_bytes());
        let small_txs_per_block = take(
            &mut errors,
            parse_or::<u64>("SPAM_SMALL_TXS_PER_BLOCK", "0"),
        );
        let fill_block_ratio = take(
            &mut errors,
            parse_non_negative_f64("SPAM_FILL_BLOCK_RATIO", "2.0"),
        );
        let floor_pool_txs = take(&mut errors, parse_or::<u64>("SPAM_FLOOR_POOL_TXS", "4000"));
        let fanout_auto = take(&mut errors, parse_bool_or("SPAM_FANOUT_AUTO", "true"));
        let enable_replaces = take(&mut errors, parse_bool_or("ENABLE_SPAM_REPLACES", "false"));
        let replaces_per_miner = take(
            &mut errors,
            parse_or::<u64>("SPAM_REPLACES_PER_MINER_PER_BLOCK", "5"),
        );
        let fallback_fee = take(
            &mut errors,
            parse_non_negative_f64("FALLBACK_FEE", "0.0001"),
        );
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

        let data_min_bytes = match data_max_bytes {
            Some(data_max_bytes) => take(&mut errors, parse_data_min_bytes(data_max_bytes)),
            None => None,
        };

        if let (
            Some(fanout_auto),
            Some(data_max_bytes),
            Some(fill_block_ratio),
            Some(fanout_utxos),
        ) = (fanout_auto, data_max_bytes, fill_block_ratio, fanout_utxos)
        {
            validate_manual_fanout(
                &mut errors,
                fanout_auto,
                data_max_bytes,
                fill_block_ratio,
                fanout_utxos,
            );
        }

        finish(errors)?;

        let (
            Some(use_raw),
            Some(fixed_txs_per_block),
            Some(fanout_utxos),
            Some(sendmany_outputs),
            Some(data_min_bytes),
            Some(data_max_bytes),
            Some(small_txs_per_block),
            Some(fill_block_ratio),
            Some(floor_pool_txs),
            Some(fanout_auto),
            Some(enable_replaces),
            Some(replaces_per_miner),
            Some(fallback_fee),
            Some(node1_url),
            Some(node2_url),
            Some(node3_url),
            Some(wallet2_name),
            Some(wallet3_name),
        ) = (
            use_raw,
            fixed_txs_per_block,
            fanout_utxos,
            sendmany_outputs,
            data_min_bytes,
            data_max_bytes,
            small_txs_per_block,
            fill_block_ratio,
            floor_pool_txs,
            fanout_auto,
            enable_replaces,
            replaces_per_miner,
            fallback_fee,
            node1_url,
            node2_url,
            node3_url,
            wallet2_name,
            wallet3_name,
        )
        else {
            unreachable!("SpamConfig fields must be present after validation");
        };

        Ok(Self {
            use_raw,
            fixed_txs_per_block,
            fanout_utxos,
            sendmany_outputs,
            data_min_bytes,
            data_max_bytes,
            small_txs_per_block,
            fill_block_ratio,
            floor_pool_txs,
            fanout_auto,
            enable_replaces,
            replaces_per_miner,
            fallback_fee,
            fee_rate_sat_vb: fallback_fee * 100_000.0,
            wallet2_name,
            wallet3_name,
            node1_url,
            node2_url,
            node3_url,
        })
    }
}

fn parse_fixed_txs_per_block() -> Result<u64, ConfigError> {
    if let Some(value) = read("SPAM_FIXED_TXS_PER_BLOCK") {
        return parse_value("SPAM_FIXED_TXS_PER_BLOCK", value);
    }
    if let Some(value) = read("SPAM_TXS_PER_BLOCK") {
        return parse_value("SPAM_TXS_PER_BLOCK", value);
    }
    if let Some(value) = read("SPAM_PER_MINER_PER_BLOCK") {
        let per_miner = parse_value::<u64>("SPAM_PER_MINER_PER_BLOCK", value)?;
        let Some(total) = per_miner.checked_mul(MINER_COUNT) else {
            return Err(ConfigError::out_of_range(
                "SPAM_PER_MINER_PER_BLOCK",
                per_miner.to_string(),
                "multiplied by miner count would overflow u64",
            ));
        };
        tracing::warn!(
            "SPAM_PER_MINER_PER_BLOCK is deprecated, set SPAM_FIXED_TXS_PER_BLOCK (total per block) instead; using {}",
            total
        );
        return Ok(total);
    }
    Ok(100)
}

fn parse_data_max_bytes() -> Result<u64, ConfigError> {
    let requested = if let Some(value) = read("SPAM_TX_DATA_MAX_BYTES") {
        parse_value("SPAM_TX_DATA_MAX_BYTES", value)?
    } else if let Some(value) = read("SPAM_TX_DATA_BYTES") {
        parse_value("SPAM_TX_DATA_BYTES", value)?
    } else {
        90_000
    };

    if requested > MAX_DATA_BYTES {
        tracing::warn!(
            "SPAM_TX_DATA_MAX_BYTES={requested} exceeds the {MAX_DATA_BYTES}-byte standard-tx limit, clamping to {MAX_DATA_BYTES}"
        );
        Ok(MAX_DATA_BYTES)
    } else {
        Ok(requested)
    }
}

fn parse_data_min_bytes(data_max_bytes: u64) -> Result<u64, ConfigError> {
    Ok(parse_or::<u64>("SPAM_TX_DATA_MIN_BYTES", "250")?.min(data_max_bytes))
}

fn parse_non_negative_f64(key: &'static str, default: &'static str) -> Result<f64, ConfigError> {
    let value = parse_or::<f64>(key, default)?;
    if !value.is_finite() || value < 0.0 {
        return Err(ConfigError::out_of_range(
            key,
            value.to_string(),
            "must be a non-negative finite number",
        ));
    }
    Ok(value)
}

fn validate_manual_fanout(
    errors: &mut Vec<ConfigError>,
    fanout_auto: bool,
    data_max_bytes: u64,
    fill_block_ratio: f64,
    fanout_utxos: u64,
) {
    if fanout_auto || data_max_bytes == 0 {
        return;
    }

    let required_min = std::cmp::max(12, (fill_block_ratio * 10.0).ceil() as u64);
    if fanout_utxos < required_min {
        errors.push(ConfigError::out_of_range(
            "SPAM_FANOUT_UTXOS",
            fanout_utxos.to_string(),
            format!(
                "is too low for SPAM_FILL_BLOCK_RATIO={fill_block_ratio}: need >= {required_min} branches (ratio x10) to hold that many blocks of unconfirmed spam, or the mempool cannot reach the target and blocks come out partial. Raise SPAM_FANOUT_UTXOS to >= {required_min}, or set SPAM_FANOUT_AUTO=true."
            ),
        ));
    }
}
