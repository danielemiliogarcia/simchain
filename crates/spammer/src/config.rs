//! Environment-driven configuration for the spammer.

use simchain_common::env_or;
use std::env;

use crate::burn::MINER_COUNT;

/// Largest OP_RETURN payload that keeps the resulting transaction below
/// Bitcoin Core's standard transaction-size limit.
const MAX_DATA_BYTES: u64 = 98_000;

/// Every setting the spammer reads from the environment. Defaults match
/// docker-compose.yml, so the binary can also run standalone.
pub struct Config {
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
    pub rpc_user: String,
    pub rpc_pass: String,
    pub wallet2_name: String,
    pub wallet3_name: String,
    pub node1_url: String,
    pub node2_url: String,
    pub node3_url: String,
}

impl Config {
    /// Load the enabled configuration. `None` preserves the original fast
    /// exit for `ENABLE_SPAM` values other than the literal `true`: unrelated
    /// settings are not parsed when the spammer is disabled.
    pub fn from_env() -> Option<Config> {
        if env_or("ENABLE_SPAM", "true") != "true" {
            return None;
        }

        // Which engine builds the spam: true (default) = raw engine, the
        // spammer signs its own transactions and the wallets are bypassed;
        // false = node wallet engine, spam goes through sendtoaddress/sendmany
        // on the miner wallets (the original behavior, kept selectable).
        let use_raw = matches!(env_or("USE_RAW_TX_SPAM", "true").as_str(), "true" | "1");

        // Fixed tx count for OUTPUT modes and the wallet engine. In
        // DATA/HYBRID mode the fill ratio drives volume instead. The two older
        // keys remain supported so existing environments retain their meaning.
        let fixed_txs_per_block: u64 = match env::var("SPAM_FIXED_TXS_PER_BLOCK")
            .or_else(|_| env::var("SPAM_TXS_PER_BLOCK"))
        {
            Ok(value) => value
                .parse()
                .expect("SPAM_FIXED_TXS_PER_BLOCK must be a positive integer"),
            Err(_) => match env::var("SPAM_PER_MINER_PER_BLOCK") {
                Ok(value) => {
                    let per_miner: u64 = value
                        .parse()
                        .expect("SPAM_PER_MINER_PER_BLOCK must be a positive integer");
                    tracing::warn!("SPAM_PER_MINER_PER_BLOCK is deprecated, set SPAM_FIXED_TXS_PER_BLOCK (total per block) instead; using {}", per_miner * MINER_COUNT);
                    per_miner * MINER_COUNT
                }
                Err(_) => 100,
            },
        };
        let fanout_utxos: u64 = env_or("SPAM_FANOUT_UTXOS", "50")
            .parse()
            .expect("SPAM_FANOUT_UTXOS must be a positive integer");
        // OUTPUT-mode fatness: 0 = sequential (one burn output per tx,
        // p2p-like arrival), N > 0 = batch (N burn outputs per tx,
        // exchange-payout-shaped). Ignored in DATA/HYBRID mode.
        let sendmany_outputs: u64 = env_or("SPAM_SENDMANY_OUTPUTS", "0")
            .parse()
            .expect("SPAM_SENDMANY_OUTPUTS must be a non-negative integer");

        // DATA/HYBRID mode (the raw-engine default) fills blocks with OP_RETURN
        // data txs. SPAM_TX_DATA_BYTES remains an alias for the renamed max key.
        let requested_data_max: u64 = env::var("SPAM_TX_DATA_MAX_BYTES")
            .or_else(|_| env::var("SPAM_TX_DATA_BYTES"))
            .unwrap_or_else(|_| "90000".to_string())
            .parse()
            .expect("SPAM_TX_DATA_MAX_BYTES must be a non-negative integer");
        let data_max_bytes = if requested_data_max > MAX_DATA_BYTES {
            tracing::warn!("SPAM_TX_DATA_MAX_BYTES={requested_data_max} exceeds the {MAX_DATA_BYTES}-byte standard-tx limit, clamping to {MAX_DATA_BYTES}");
            MAX_DATA_BYTES
        } else {
            requested_data_max
        };
        // 0 or >= MAX makes every data tx exactly MAX. A value below MAX
        // (default 250) yields a log-uniform realistic size distribution.
        let data_min_bytes: u64 = env_or("SPAM_TX_DATA_MIN_BYTES", "250")
            .parse::<u64>()
            .expect("SPAM_TX_DATA_MIN_BYTES must be a non-negative integer")
            .min(data_max_bytes);

        let small_txs_per_block: u64 = env_or("SPAM_SMALL_TXS_PER_BLOCK", "0")
            .parse()
            .expect("SPAM_SMALL_TXS_PER_BLOCK must be a non-negative integer");
        let fill_block_ratio: f64 = env_or("SPAM_FILL_BLOCK_RATIO", "2.0")
            .parse()
            .expect("SPAM_FILL_BLOCK_RATIO must be a number");
        let floor_pool_txs: u64 = env_or("SPAM_FLOOR_POOL_TXS", "4000")
            .parse()
            .expect("SPAM_FLOOR_POOL_TXS must be a non-negative integer");
        let fanout_auto = matches!(env_or("SPAM_FANOUT_AUTO", "true").as_str(), "true" | "1");
        let enable_replaces = matches!(
            env_or("ENABLE_SPAM_REPLACES", "false").as_str(),
            "true" | "1"
        );
        let replaces_per_miner: u64 = env_or("SPAM_REPLACES_PER_MINER_PER_BLOCK", "5")
            .parse()
            .expect("SPAM_REPLACES_PER_MINER_PER_BLOCK must be a non-negative integer");
        // FALLBACK_FEE is BTC/kvB; the engines work in sat/vB.
        let fallback_fee: f64 = env_or("FALLBACK_FEE", "0.0001")
            .parse()
            .expect("FALLBACK_FEE must be a number (BTC/kvB)");

        Some(Config {
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
            rpc_user: env_or("BTC_RPC_USER", "foo"),
            rpc_pass: env_or("BTC_RPC_PASS", "rpcpassword"),
            wallet2_name: env_or("NODE2_WALLET_NAME", "node2"),
            wallet3_name: env_or("NODE3_WALLET_NAME", "node3"),
            node1_url: env_or("NODE1_RPC_URL", "http://btc-simnet-node1:18443"),
            node2_url: env_or("NODE2_RPC_URL", "http://btc-simnet-node2:18443"),
            node3_url: env_or("NODE3_RPC_URL", "http://btc-simnet-node3:18443"),
        })
    }

    /// Node 2 takes the odd remainder so the shares always sum to the total.
    pub fn fixed_shares(&self) -> (u64, u64) {
        (
            self.fixed_txs_per_block.div_ceil(MINER_COUNT),
            self.fixed_txs_per_block / MINER_COUNT,
        )
    }
}
