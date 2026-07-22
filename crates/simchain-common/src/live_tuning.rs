//! Shared live-retune settings: the catalog, parsing, and validation used by
//! the resident workers and control plane.
//!
//! The mining controller and the spammer validate their boot environment
//! through this module, and the control plane validates proposed settings
//! through it too, so a configuration the control plane accepts is exactly a configuration the
//! workers accept through their policy API. Parsing is source-agnostic so the
//! same rules apply at boot and during a live transaction.
//!
//! For required settings, unset and empty both select the catalog default.
//! Optional mining bounds preserve an explicit empty value as "unbounded"
//! while an unset value still receives the catalog default.

use crate::config::ConfigError;
use std::collections::BTreeMap;
use std::time::Duration;

/// Wallets the spam is split across (node2 and node3). Shared so the legacy
/// per-miner alias converts identically everywhere.
pub const MINER_COUNT: u64 = 2;

/// Largest OP_RETURN payload that keeps the resulting transaction below
/// Bitcoin Core's standard transaction-size limit.
pub const MAX_DATA_BYTES: u64 = 98_000;

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// Key/value lookup the parsers read from: the process environment for the
/// workers, or an in-memory desired-state map for the control plane.
pub trait TuningSource {
    fn get(&self, key: &str) -> Option<String>;
}

/// Reads from the process environment (tool binaries).
pub struct EnvSource;

impl TuningSource for EnvSource {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl TuningSource for BTreeMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        BTreeMap::get(self, key).cloned()
    }
}

impl TuningSource for std::collections::HashMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        std::collections::HashMap::get(self, key).cloned()
    }
}

/// Required-value lookup: unset and empty both mean "use the default".
fn value_or(source: &dyn TuningSource, key: &str, default: &str) -> String {
    match source.get(key) {
        Some(value) if !value.trim().is_empty() => value,
        _ => default.to_string(),
    }
}

fn non_empty(source: &dyn TuningSource, key: &str) -> Option<String> {
    source.get(key).filter(|value| !value.trim().is_empty())
}

fn parse<T>(key: &'static str, value: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .trim()
        .parse::<T>()
        .map_err(|error: T::Err| ConfigError::invalid(key, value, error.to_string()))
}

fn parse_or<T>(
    source: &dyn TuningSource,
    key: &'static str,
    default: &str,
) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    parse(key, &value_or(source, key, default))
}

fn parse_bool(key: &'static str, value: &str) -> Result<bool, ConfigError> {
    match value.trim() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ConfigError::invalid(
            key,
            value,
            "expected one of: true, false, 1, 0",
        )),
    }
}

fn parse_optional<T>(source: &dyn TuningSource, key: &'static str) -> Result<Option<T>, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match non_empty(source, key) {
        Some(value) => parse(key, &value).map(Some),
        None => Ok(None),
    }
}

fn collect<T>(errors: &mut Vec<ConfigError>, result: Result<T, ConfigError>) -> Option<T> {
    crate::config::take(errors, result)
}

// ---------------------------------------------------------------------------
// Mining subset
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockIntervalMode {
    Fixed,
    Poisson,
}

impl BlockIntervalMode {
    pub fn is_poisson(self) -> bool {
        matches!(self, BlockIntervalMode::Poisson)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BlockIntervalMode::Fixed => "fixed",
            BlockIntervalMode::Poisson => "poisson",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct MinerWeights {
    pub node2: u64,
    pub node3: u64,
    pub total: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
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

/// The live-retunable mining-controller subset, validated.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct MiningTuning {
    pub interval_mode: BlockIntervalMode,
    pub mean_secs: u64,
    pub interval_bounds: IntervalBounds,
    pub miner_weights: Option<MinerWeights>,
    pub rng_seed: Option<u64>,
}

impl MiningTuning {
    /// Parse and validate from `source`, enforcing exactly the rules the
    /// mining controller enforces at startup. Collects every error.
    pub fn from_source(source: &dyn TuningSource) -> Result<Self, ConfigError> {
        let mut errors = Vec::new();
        let interval_mode = collect(&mut errors, parse_interval_mode(source));
        let mean_secs = collect(
            &mut errors,
            parse_positive_u64(source, "BLOCK_INTERVAL_MEAN_SECS", "15"),
        );
        let interval_bounds = collect(&mut errors, parse_interval_bounds(source));
        let miner_weights = collect(&mut errors, parse_miner_weights(source));
        let rng_seed = collect(
            &mut errors,
            parse_optional::<u64>(source, "MINING_RNG_SEED"),
        );

        if let (Some(mean_secs), Some(interval_mode), Some(interval_bounds)) =
            (mean_secs, interval_mode, interval_bounds)
        {
            if interval_mode.is_poisson() {
                validate_poisson_mean(&mut errors, mean_secs, interval_bounds);
            }
        }

        crate::config::finish(errors)?;

        let (
            Some(interval_mode),
            Some(mean_secs),
            Some(interval_bounds),
            Some(miner_weights),
            Some(rng_seed),
        ) = (
            interval_mode,
            mean_secs,
            interval_bounds,
            miner_weights,
            rng_seed,
        )
        else {
            unreachable!("MiningTuning fields must be present after validation");
        };

        Ok(Self {
            interval_mode,
            mean_secs,
            interval_bounds,
            miner_weights,
            rng_seed,
        })
    }

    /// Canonical env-string form of every mining-scope managed key.
    pub fn canonical_values(&self) -> BTreeMap<&'static str, String> {
        let mut values = BTreeMap::new();
        values.insert(
            "BLOCK_INTERVAL_MODE",
            self.interval_mode.as_str().to_string(),
        );
        values.insert("BLOCK_INTERVAL_MEAN_SECS", self.mean_secs.to_string());
        values.insert(
            "BLOCK_INTERVAL_MIN_SECS",
            self.interval_bounds
                .min
                .map_or(String::new(), |v| v.to_string()),
        );
        values.insert(
            "BLOCK_INTERVAL_MAX_SECS",
            self.interval_bounds
                .max
                .map_or(String::new(), |v| v.to_string()),
        );
        values.insert(
            "MINER_WEIGHTS",
            self.miner_weights
                .map_or(String::new(), |w| format!("{},{}", w.node2, w.node3)),
        );
        values.insert(
            "MINING_RNG_SEED",
            self.rng_seed.map_or(String::new(), |s| s.to_string()),
        );
        values
    }
}

fn parse_interval_mode(source: &dyn TuningSource) -> Result<BlockIntervalMode, ConfigError> {
    let value = value_or(source, "BLOCK_INTERVAL_MODE", "poisson");
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

fn parse_positive_u64(
    source: &dyn TuningSource,
    key: &'static str,
    default: &str,
) -> Result<u64, ConfigError> {
    let value = parse_or::<u64>(source, key, default)?;
    if value == 0 {
        return Err(ConfigError::out_of_range(
            key,
            value.to_string(),
            "must be a positive integer",
        ));
    }
    Ok(value)
}

fn parse_interval_bound(
    source: &dyn TuningSource,
    key: &'static str,
) -> Result<Option<f64>, ConfigError> {
    let Some(seconds) = parse_optional::<f64>(source, key)? else {
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

fn parse_interval_bounds(source: &dyn TuningSource) -> Result<IntervalBounds, ConfigError> {
    let mut errors = Vec::new();
    let min = collect(
        &mut errors,
        parse_interval_bound(source, "BLOCK_INTERVAL_MIN_SECS"),
    );
    let max = collect(
        &mut errors,
        parse_interval_bound(source, "BLOCK_INTERVAL_MAX_SECS"),
    );

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

    crate::config::finish(errors)?;

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

fn parse_miner_weights(source: &dyn TuningSource) -> Result<Option<MinerWeights>, ConfigError> {
    let Some(value) = non_empty(source, "MINER_WEIGHTS") else {
        return Ok(None);
    };

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

    let node2 = parse_weight(parts[0], &value)?;
    let node3 = parse_weight(parts[1], &value)?;
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

fn parse_weight(part: &str, full_value: &str) -> Result<u64, ConfigError> {
    part.parse::<u64>().map_err(|error| {
        ConfigError::invalid(
            "MINER_WEIGHTS",
            full_value.to_string(),
            format!("expected two non-negative integers, e.g. 70,30 ({error})"),
        )
    })
}

// ---------------------------------------------------------------------------
// Spam subset
// ---------------------------------------------------------------------------

/// The live-retunable spammer subset, validated.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SpamTuning {
    pub enabled: bool,
    /// Always true: the node-wallet engine is deprecated and no longer
    /// selectable. The field remains for policy-JSON compatibility.
    pub use_raw: bool,
    /// Spam fee floor in BTC/kvB. Distinct from the nodes' boot-time
    /// `-fallbackfee`: this one retunes live; the node flag is fixed until the
    /// containers are recreated. The alias accepts policies persisted before
    /// the split.
    #[serde(alias = "fallback_fee")]
    pub spam_fee: f64,
    pub fixed_txs_per_block: u64,
    pub sendmany_outputs: u64,
    /// Clamped to [`MAX_DATA_BYTES`], like the spammer clamps at startup.
    pub data_max_bytes: u64,
    /// As configured; the effective value is [`Self::effective_data_min_bytes`].
    pub data_min_bytes: u64,
    pub small_txs_per_block: u64,
    pub floor_pool_txs: u64,
    pub fill_block_ratio: f64,
    pub fanout_auto: bool,
    pub fanout_utxos: u64,
    pub enable_replaces: bool,
    pub replaces_per_miner: u64,
}

impl SpamTuning {
    pub const BULK_FEE_PREMIUM_SAT_VB: f64 = 1.0;
    pub const FANOUT_FEE_MULTIPLIER: u64 = 2;
    pub const RBF_FEE_MULTIPLIER: u64 = 2;
    /// Parse and validate from `source`, enforcing exactly the rules the
    /// spammer enforces at startup (including the deprecated boot aliases
    /// `SPAM_TXS_PER_BLOCK`, `SPAM_PER_MINER_PER_BLOCK` and
    /// `SPAM_TX_DATA_BYTES`). Returns the tuning plus human-readable
    /// warnings (deprecations, clamps) for the caller to log or display.
    pub fn from_source(source: &dyn TuningSource) -> Result<(Self, Vec<String>), ConfigError> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        let enabled = spam_enabled(source);
        // USE_RAW_TX_SPAM is pinned to the raw engine: the node-wallet engine
        // is deprecated and no longer selectable. A legacy explicit false is
        // overridden with a warning instead of failing the configuration.
        if matches!(
            non_empty(source, "USE_RAW_TX_SPAM")
                .as_deref()
                .map(str::trim),
            Some("false") | Some("0")
        ) {
            warnings.push(
                "USE_RAW_TX_SPAM=false (node-wallet engine) is deprecated and ignored; the raw engine is always used".to_string(),
            );
        }
        let fixed_txs_per_block = collect(
            &mut errors,
            parse_fixed_txs_per_block(source, &mut warnings),
        );
        let fanout_utxos = collect(
            &mut errors,
            parse_or::<u64>(source, "SPAM_FANOUT_UTXOS", "50"),
        );
        let sendmany_outputs = collect(
            &mut errors,
            parse_or::<u64>(source, "SPAM_SENDMANY_OUTPUTS", "0"),
        );
        let data_max_bytes = collect(&mut errors, parse_data_max_bytes(source, &mut warnings));
        let data_min_bytes = collect(
            &mut errors,
            parse_or::<u64>(source, "SPAM_TX_DATA_MIN_BYTES", "250"),
        );
        let small_txs_per_block = collect(
            &mut errors,
            parse_or::<u64>(source, "SPAM_SMALL_TXS_PER_BLOCK", "0"),
        );
        let fill_block_ratio = collect(
            &mut errors,
            parse_non_negative_f64(source, "SPAM_FILL_BLOCK_RATIO", "2.0"),
        );
        let floor_pool_txs = collect(
            &mut errors,
            parse_or::<u64>(source, "SPAM_FLOOR_POOL_TXS", "4000"),
        );
        let fanout_auto = collect(
            &mut errors,
            parse_bool_or(source, "SPAM_FANOUT_AUTO", "true"),
        );
        let enable_replaces = collect(
            &mut errors,
            parse_bool_or(source, "ENABLE_SPAM_REPLACES", "false"),
        );
        let replaces_per_miner = collect(
            &mut errors,
            parse_or::<u64>(source, "SPAM_REPLACES_PER_MINER_PER_BLOCK", "5"),
        );
        let spam_fee = collect(&mut errors, parse_spam_fee(source, &mut warnings));

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

        crate::config::finish(errors)?;

        let (
            Some(fixed_txs_per_block),
            Some(fanout_utxos),
            Some(sendmany_outputs),
            Some(data_max_bytes),
            Some(data_min_bytes),
            Some(small_txs_per_block),
            Some(fill_block_ratio),
            Some(floor_pool_txs),
            Some(fanout_auto),
            Some(enable_replaces),
            Some(replaces_per_miner),
            Some(spam_fee),
        ) = (
            fixed_txs_per_block,
            fanout_utxos,
            sendmany_outputs,
            data_max_bytes,
            data_min_bytes,
            small_txs_per_block,
            fill_block_ratio,
            floor_pool_txs,
            fanout_auto,
            enable_replaces,
            replaces_per_miner,
            spam_fee,
        )
        else {
            unreachable!("SpamTuning fields must be present after validation");
        };

        Ok((
            Self {
                enabled,
                use_raw: true,
                spam_fee,
                fixed_txs_per_block,
                sendmany_outputs,
                data_max_bytes,
                data_min_bytes,
                small_txs_per_block,
                floor_pool_txs,
                fill_block_ratio,
                fanout_auto,
                fanout_utxos,
                enable_replaces,
                replaces_per_miner,
            },
            warnings,
        ))
    }

    /// The data floor the engines actually use: never above the max.
    pub fn effective_data_min_bytes(&self) -> u64 {
        self.data_min_bytes.min(self.data_max_bytes)
    }

    /// The raw engine's fee rate in sat/vB derived from the BTC/kvB fee.
    pub fn fee_rate_sat_vb(&self) -> f64 {
        self.spam_fee * 100_000.0
    }

    /// Conservative upper bound for fees generated by either spam engine,
    /// including bulk rounding, refill fan-out, and one RBF replacement.
    pub fn max_generated_feerate_sat_vb(&self) -> f64 {
        Self::RBF_FEE_MULTIPLIER as f64 * (self.fee_rate_sat_vb() + 2.0)
    }

    /// Minimum independent branches per raw engine needed to sustain the
    /// configured DATA/HYBRID mempool depth without hitting ancestor limits.
    pub fn minimum_data_fanout(&self) -> u64 {
        std::cmp::max(12, (self.fill_block_ratio * 10.0).ceil() as u64)
    }

    /// Preferred branch count per raw engine. Automatic mode carries 50%
    /// headroom above the minimum; manual mode uses the configured target.
    pub fn desired_data_fanout(&self) -> u64 {
        if self.fanout_auto {
            std::cmp::max(12, (self.fill_block_ratio * 15.0).ceil() as u64)
        } else {
            self.fanout_utxos
        }
    }

    /// Canonical env-string form of every spam-scope managed key.
    pub fn canonical_values(&self) -> BTreeMap<&'static str, String> {
        let mut values = BTreeMap::new();
        values.insert("ENABLE_SPAM", bool_str(self.enabled).to_string());
        values.insert("SPAM_FEE", self.spam_fee.to_string());
        values.insert(
            "SPAM_FIXED_TXS_PER_BLOCK",
            self.fixed_txs_per_block.to_string(),
        );
        values.insert("SPAM_SENDMANY_OUTPUTS", self.sendmany_outputs.to_string());
        values.insert("SPAM_TX_DATA_MAX_BYTES", self.data_max_bytes.to_string());
        values.insert("SPAM_TX_DATA_MIN_BYTES", self.data_min_bytes.to_string());
        values.insert(
            "SPAM_SMALL_TXS_PER_BLOCK",
            self.small_txs_per_block.to_string(),
        );
        values.insert("SPAM_FLOOR_POOL_TXS", self.floor_pool_txs.to_string());
        values.insert("SPAM_FILL_BLOCK_RATIO", self.fill_block_ratio.to_string());
        values.insert("SPAM_FANOUT_AUTO", bool_str(self.fanout_auto).to_string());
        values.insert("SPAM_FANOUT_UTXOS", self.fanout_utxos.to_string());
        values.insert(
            "ENABLE_SPAM_REPLACES",
            bool_str(self.enable_replaces).to_string(),
        );
        values.insert(
            "SPAM_REPLACES_PER_MINER_PER_BLOCK",
            self.replaces_per_miner.to_string(),
        );
        values
    }
}

/// `ENABLE_SPAM` keeps the spammer's exact semantics: only the literal string
/// `true` enables spam (`1` does not).
pub fn spam_enabled(source: &dyn TuningSource) -> bool {
    value_or(source, "ENABLE_SPAM", "true") == "true"
}

fn bool_str(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn parse_bool_or(
    source: &dyn TuningSource,
    key: &'static str,
    default: &str,
) -> Result<bool, ConfigError> {
    parse_bool(key, &value_or(source, key, default))
}

fn parse_fixed_txs_per_block(
    source: &dyn TuningSource,
    warnings: &mut Vec<String>,
) -> Result<u64, ConfigError> {
    if let Some(value) = non_empty(source, "SPAM_FIXED_TXS_PER_BLOCK") {
        return parse("SPAM_FIXED_TXS_PER_BLOCK", &value);
    }
    if let Some(value) = non_empty(source, "SPAM_TXS_PER_BLOCK") {
        return parse("SPAM_TXS_PER_BLOCK", &value);
    }
    if let Some(value) = non_empty(source, "SPAM_PER_MINER_PER_BLOCK") {
        let per_miner = parse::<u64>("SPAM_PER_MINER_PER_BLOCK", &value)?;
        let Some(total) = per_miner.checked_mul(MINER_COUNT) else {
            return Err(ConfigError::out_of_range(
                "SPAM_PER_MINER_PER_BLOCK",
                per_miner.to_string(),
                "multiplied by miner count would overflow u64",
            ));
        };
        warnings.push(format!(
            "SPAM_PER_MINER_PER_BLOCK is deprecated, set SPAM_FIXED_TXS_PER_BLOCK (total per block) instead; using {total}"
        ));
        return Ok(total);
    }
    Ok(100)
}

fn parse_data_max_bytes(
    source: &dyn TuningSource,
    warnings: &mut Vec<String>,
) -> Result<u64, ConfigError> {
    let requested = if let Some(value) = non_empty(source, "SPAM_TX_DATA_MAX_BYTES") {
        parse::<u64>("SPAM_TX_DATA_MAX_BYTES", &value)?
    } else if let Some(value) = non_empty(source, "SPAM_TX_DATA_BYTES") {
        parse::<u64>("SPAM_TX_DATA_BYTES", &value)?
    } else {
        90_000
    };

    if requested > MAX_DATA_BYTES {
        warnings.push(format!(
            "SPAM_TX_DATA_MAX_BYTES={requested} exceeds the {MAX_DATA_BYTES}-byte standard-tx limit, clamping to {MAX_DATA_BYTES}"
        ));
        Ok(MAX_DATA_BYTES)
    } else {
        Ok(requested)
    }
}

/// `SPAM_FEE` with a legacy fallback: before the split the single
/// `FALLBACK_FEE` variable both set the nodes' boot `-fallbackfee` and
/// retuned the spammer. An environment that still sets only `FALLBACK_FEE`
/// keeps its spam fee, with a warning to migrate.
fn parse_spam_fee(
    source: &dyn TuningSource,
    warnings: &mut Vec<String>,
) -> Result<f64, ConfigError> {
    if non_empty(source, "SPAM_FEE").is_none() {
        if let Some(value) = non_empty(source, "FALLBACK_FEE") {
            warnings.push(format!(
                "SPAM_FEE is unset; using legacy FALLBACK_FEE={value} as the spam fee. FALLBACK_FEE now only sets the nodes' boot-time -fallbackfee; set SPAM_FEE to retune spam."
            ));
            return parse_non_negative_f64_value("FALLBACK_FEE", &value);
        }
    }
    parse_non_negative_f64(source, "SPAM_FEE", "0.0001")
}

fn parse_non_negative_f64(
    source: &dyn TuningSource,
    key: &'static str,
    default: &str,
) -> Result<f64, ConfigError> {
    parse_non_negative_f64_value(key, &value_or(source, key, default))
}

fn parse_non_negative_f64_value(key: &'static str, raw: &str) -> Result<f64, ConfigError> {
    let value = parse::<f64>(key, raw)?;
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

// ---------------------------------------------------------------------------
// Combined view and catalog
// ---------------------------------------------------------------------------

/// Both live-retunable subsets, validated together (all errors collected).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveTuning {
    pub mining: MiningTuning,
    pub spam: SpamTuning,
}

impl LiveTuning {
    pub fn from_source(source: &dyn TuningSource) -> Result<(Self, Vec<String>), ConfigError> {
        let mining = MiningTuning::from_source(source);
        let spam = SpamTuning::from_source(source);
        match (mining, spam) {
            (Ok(mining), Ok((spam, warnings))) => Ok((Self { mining, spam }, warnings)),
            (mining, spam) => {
                let mut errors = Vec::new();
                if let Err(error) = mining {
                    errors.push(error);
                }
                if let Err(error) = spam {
                    errors.push(error);
                }
                Err(ConfigError::aggregate(errors)
                    .expect("at least one tuning error must be present"))
            }
        }
    }

    /// Canonical env-string form of the full managed set, in catalog order.
    pub fn canonical_values(&self) -> BTreeMap<&'static str, String> {
        let mut values = self.mining.canonical_values();
        values.append(&mut self.spam.canonical_values());
        values
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingGroup {
    Mining,
    SpamBasics,
    SpamAdvanced,
}

impl SettingGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            SettingGroup::Mining => "mining",
            SettingGroup::SpamBasics => "spam-basics",
            SettingGroup::SpamAdvanced => "spam-advanced",
        }
    }
}

/// Resident worker that owns a setting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceScope {
    MiningController,
    Spammer,
}

impl ServiceScope {
    pub fn component_name(self) -> &'static str {
        match self {
            ServiceScope::MiningController => "mining",
            ServiceScope::Spammer => "spam",
        }
    }
}

/// How a UI should render the control for a setting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlKind {
    Toggle,
    Integer,
    Decimal,
    Text,
    Choice(&'static [&'static str]),
}

impl ControlKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlKind::Toggle => "toggle",
            ControlKind::Integer => "integer",
            ControlKind::Decimal => "decimal",
            ControlKind::Text => "text",
            ControlKind::Choice(_) => "choice",
        }
    }
}

/// One runtime-managed variable: metadata for schema/UI plus its boot default.
pub struct SettingSpec {
    pub key: &'static str,
    pub default: &'static str,
    pub group: SettingGroup,
    pub scope: ServiceScope,
    pub control: ControlKind,
    /// Empty input is allowed and means "unset" (kept as an empty string).
    pub optional: bool,
    pub help: &'static str,
    pub warning: Option<&'static str>,
}

/// The runtime-managed catalog, in display order.
pub const MANAGED_SETTINGS: &[SettingSpec] = &[
    SettingSpec {
        key: "BLOCK_INTERVAL_MODE",
        default: "poisson",
        group: SettingGroup::Mining,
        scope: ServiceScope::MiningController,
        control: ControlKind::Choice(&["poisson", "fixed"]),
        optional: false,
        help: "Block interval distribution: poisson (exponential, mainnet-like) or fixed (always the mean). Use poisson to test variable confirmation latency; use fixed for a predictable mining cadence.",
        warning: None,
    },
    SettingSpec {
        key: "BLOCK_INTERVAL_MEAN_SECS",
        default: "15",
        group: SettingGroup::Mining,
        scope: ServiceScope::MiningController,
        control: ControlKind::Integer,
        optional: false,
        help: "Mean seconds between blocks (positive integer). Controls simulation speed and expected confirmation latency.",
        warning: None,
    },
    SettingSpec {
        key: "BLOCK_INTERVAL_MIN_SECS",
        default: "10",
        group: SettingGroup::Mining,
        scope: ServiceScope::MiningController,
        control: ControlKind::Decimal,
        optional: true,
        help: "Lower clamp on poisson-sampled intervals; empty = unbounded and fixed mode ignores it. Prevents unusually fast consecutive blocks in bounded demonstrations.",
        warning: None,
    },
    SettingSpec {
        key: "BLOCK_INTERVAL_MAX_SECS",
        default: "20",
        group: SettingGroup::Mining,
        scope: ServiceScope::MiningController,
        control: ControlKind::Decimal,
        optional: true,
        help: "Upper clamp on poisson-sampled intervals; empty = unbounded and fixed mode ignores it. Prevents a long poisson tail from stalling a short test run.",
        warning: None,
    },
    SettingSpec {
        key: "MINER_WEIGHTS",
        default: "",
        group: SettingGroup::Mining,
        scope: ServiceScope::MiningController,
        control: ControlKind::Text,
        optional: true,
        help: "Relative node2,node3 hashrates, e.g. 70,30; empty = strict alternation. Models unequal miner hashpower and biases which miner produces each block.",
        warning: None,
    },
    SettingSpec {
        key: "MINING_RNG_SEED",
        default: "",
        group: SettingGroup::Mining,
        scope: ServiceScope::MiningController,
        control: ControlKind::Integer,
        optional: true,
        help: "Unsigned 64-bit decimal seed for reproducible stochastic timing and miner selection. Example: 42; valid range: 0 to 18446744073709551615. Empty = generate a random seed.",
        warning: None,
    },
    SettingSpec {
        key: "ENABLE_SPAM",
        default: "true",
        group: SettingGroup::SpamBasics,
        scope: ServiceScope::Spammer,
        control: ControlKind::Toggle,
        optional: false,
        help: "Enable spam generation. When false the worker and raw engine remain resident, preserving branch and floor-pool state for a fast re-enable. Controls whether mined blocks carry background transaction load. Other spam settings are ignored and disabled while false.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_FEE",
        default: "0.0001",
        group: SettingGroup::SpamBasics,
        scope: ServiceScope::Spammer,
        control: ControlKind::Decimal,
        optional: false,
        help: "Spam fee floor in BTC/kvB: 0.0001 = 10 sat/vB, 0.001 = 100 sat/vB, and 0.1 = 10,000 sat/vB. Floor fills pay exactly this; bulk spam pays a small premium. Higher fees also multiply the BTC needed by every raw-engine branch. For example, with 90,000-byte DATA transactions, 0.1 needs about 144 BTC per branch and about 8,650 BTC per miner at the ratio-4 auto-fanout target. An unaffordable combination of fee, payload size, and fanout causes capacity_degraded: provisioning keeps trying but cannot recover until demand is reduced or more mature funds are available. It can also drain spendable miner treasuries below the faucet reserve, making faucet capacity zero until funds recover or mined fees mature. Applies in place at a safe transaction boundary while preserving tracked funds.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_FILL_BLOCK_RATIO",
        default: "2.0",
        group: SettingGroup::SpamBasics,
        scope: ServiceScope::Spammer,
        control: ControlKind::Decimal,
        optional: false,
        help: "DATA/HYBRID fill target in blocks of mempool weight: 0.5 = half-full blocks, 2 = full + backlog. Controls block fullness and how much pending traffic remains visible after a block. An increase triggers one immediate mempool-deficit catch-up without resetting the engine.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_TX_DATA_MAX_BYTES",
        default: "90000",
        group: SettingGroup::SpamBasics,
        scope: ServiceScope::Spammer,
        control: ControlKind::Integer,
        optional: false,
        help: "Biggest OP_RETURN payload for DATA/HYBRID fill; 0 switches to the legacy OUTPUT mode. Larger payloads fill blocks with fewer transactions without growing the spendable UTXO set.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_TX_DATA_MIN_BYTES",
        default: "250",
        group: SettingGroup::SpamBasics,
        scope: ServiceScope::Spammer,
        control: ControlKind::Integer,
        optional: false,
        help: "Smallest OP_RETURN payload; sizes spread log-uniformly between min and max. Controls visible transaction-size diversity; a lower minimum produces more small transactions.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_SMALL_TXS_PER_BLOCK",
        default: "0",
        group: SettingGroup::SpamAdvanced,
        scope: ServiceScope::Spammer,
        control: ControlKind::Integer,
        optional: false,
        help: "Extra minimum-size floor-priced txs per block on top of the data fill; 0 = none. Controls how realistic the transaction-size mixture looks.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_FLOOR_POOL_TXS",
        default: "4000",
        group: SettingGroup::SpamAdvanced,
        scope: ServiceScope::Spammer,
        control: ControlKind::Integer,
        optional: false,
        help: "Standing floor-priced ~110-vB self-transfers kept in the mempool (airtight fee floor); 0 = off. When blocks are full, prevents cheap transactions from slipping through residual gaps.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_FIXED_TXS_PER_BLOCK",
        default: "100",
        group: SettingGroup::SpamAdvanced,
        scope: ServiceScope::Spammer,
        control: ControlKind::Integer,
        optional: false,
        help: "Fixed tx count for OUTPUT modes and the wallet engine; ignored in DATA/HYBRID mode. Controls visible transaction count and node workload when using OUTPUT mode.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_SENDMANY_OUTPUTS",
        default: "0",
        group: SettingGroup::SpamAdvanced,
        scope: ServiceScope::Spammer,
        control: ControlKind::Integer,
        optional: false,
        help: "OUTPUT-mode fatness: 0 = sequential txs, N = batches of N burn outputs per tx. Higher values model payout batches and fill block weight with fewer transaction IDs, at greater UTXO cost.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_FANOUT_AUTO",
        default: "true",
        group: SettingGroup::SpamAdvanced,
        scope: ServiceScope::Spammer,
        control: ControlKind::Toggle,
        optional: false,
        help: "Auto-size the branch pool from the fill ratio; false = use SPAM_FANOUT_UTXOS. The minimum is ratio x10 and the preferred target is ratio x15, so existing headroom stays active while extra branches are provisioned in the background.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_FANOUT_UTXOS",
        default: "50",
        group: SettingGroup::SpamAdvanced,
        scope: ServiceScope::Spammer,
        control: ControlKind::Integer,
        optional: false,
        help: "Manual preferred branch-pool size; must cover the fill ratio (>= ratio x10, min 12) when auto is off. Independent branches bypass unconfirmed-chain limits; usable branches keep sending while added capacity confirms in the background.",
        warning: None,
    },
    SettingSpec {
        key: "ENABLE_SPAM_REPLACES",
        default: "false",
        group: SettingGroup::SpamAdvanced,
        scope: ServiceScope::Spammer,
        control: ControlKind::Toggle,
        optional: false,
        help: "Fee-bump (RBF) a fraction of the just-sent spam so the mempool carries real BIP125 replacements. Exercises replacement handling in explorers, wallets, and transaction monitors.",
        warning: None,
    },
    SettingSpec {
        key: "SPAM_REPLACES_PER_MINER_PER_BLOCK",
        default: "5",
        group: SettingGroup::SpamAdvanced,
        scope: ServiceScope::Spammer,
        control: ControlKind::Integer,
        optional: false,
        help: "How many of each miner's spam txs get fee-bumped per block when RBF traffic is enabled. Controls replacement-event density and downstream processing load. Ignored while ENABLE_SPAM_REPLACES=false.",
        warning: None,
    },
];

pub fn spec(key: &str) -> Option<&'static SettingSpec> {
    MANAGED_SETTINGS.iter().find(|spec| spec.key == key)
}

pub fn is_managed_key(key: &str) -> bool {
    spec(key).is_some()
}

/// The full staged map: catalog defaults overlaid with managed entries.
/// Explicit empty values survive for optional settings; for required settings
/// empty means the catalog default.
pub fn staged_map(overrides: &dyn TuningSource) -> BTreeMap<String, String> {
    MANAGED_SETTINGS
        .iter()
        .map(|spec| {
            let value = match overrides.get(spec.key) {
                Some(value) if value.trim().is_empty() && spec.optional => String::new(),
                Some(value) if !value.trim().is_empty() => value,
                _ => spec.default.to_string(),
            };
            (spec.key.to_string(), value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_validate_and_canonicalize() {
        let source = staged_map(&BTreeMap::new());
        let (tuning, warnings) = LiveTuning::from_source(&source).expect("defaults must be valid");
        assert!(warnings.is_empty());
        assert_eq!(tuning.mining.mean_secs, 15);
        assert_eq!(tuning.mining.interval_mode, BlockIntervalMode::Poisson);
        assert_eq!(tuning.mining.interval_bounds.min, Some(10.0));
        assert_eq!(tuning.mining.interval_bounds.max, Some(20.0));
        assert!(tuning.spam.enabled);
        assert!(tuning.spam.use_raw);
        assert_eq!(tuning.spam.fixed_txs_per_block, 100);

        let canonical = tuning.canonical_values();
        assert_eq!(canonical["BLOCK_INTERVAL_MODE"], "poisson");
        assert_eq!(canonical["SPAM_FILL_BLOCK_RATIO"], "2");
        assert_eq!(canonical["SPAM_FEE"], "0.0001");
        assert_eq!(canonical["MINER_WEIGHTS"], "");
        assert_eq!(canonical.len(), MANAGED_SETTINGS.len());
    }

    #[test]
    fn empty_values_fall_back_to_defaults() {
        let source = map(&[("BLOCK_INTERVAL_MEAN_SECS", ""), ("ENABLE_SPAM", " ")]);
        let (tuning, _) = LiveTuning::from_source(&source).expect("empty means default");
        assert_eq!(tuning.mining.mean_secs, 15);
        assert!(tuning.spam.enabled);
    }

    #[test]
    fn staged_map_preserves_explicit_empty_optional_values() {
        let staged = staged_map(&map(&[
            ("BLOCK_INTERVAL_MIN_SECS", ""),
            ("BLOCK_INTERVAL_MODE", ""),
        ]));
        assert_eq!(staged["BLOCK_INTERVAL_MIN_SECS"], "");
        assert_eq!(staged["BLOCK_INTERVAL_MODE"], "poisson");
        let mining = MiningTuning::from_source(&staged).expect("empty bound is unbounded");
        assert_eq!(mining.interval_bounds.min, None);
    }

    #[test]
    fn enable_spam_requires_literal_true() {
        // The spammer only enables on the literal string "true".
        assert!(spam_enabled(&map(&[("ENABLE_SPAM", "true")])));
        assert!(!spam_enabled(&map(&[("ENABLE_SPAM", "1")])));
        assert!(!spam_enabled(&map(&[("ENABLE_SPAM", "false")])));
    }

    #[test]
    fn poisson_mean_outside_bounds_is_rejected() {
        let source = map(&[
            ("BLOCK_INTERVAL_MEAN_SECS", "30"),
            ("BLOCK_INTERVAL_MIN_SECS", "10"),
            ("BLOCK_INTERVAL_MAX_SECS", "20"),
        ]);
        let error = MiningTuning::from_source(&source).unwrap_err();
        assert!(error.to_string().contains("BLOCK_INTERVAL_MAX_SECS"));
    }

    #[test]
    fn fixed_mean_outside_bounds_is_allowed() {
        let source = map(&[
            ("BLOCK_INTERVAL_MODE", "fixed"),
            ("BLOCK_INTERVAL_MEAN_SECS", "30"),
            ("BLOCK_INTERVAL_MIN_SECS", "10"),
            ("BLOCK_INTERVAL_MAX_SECS", "20"),
        ]);
        MiningTuning::from_source(&source).expect("fixed mode skips the poisson mean check");
    }

    #[test]
    fn zero_weights_are_rejected() {
        let source = map(&[("MINER_WEIGHTS", "0,0")]);
        let error = MiningTuning::from_source(&source).unwrap_err();
        assert!(error.to_string().contains("must not be 0,0"));
    }

    #[test]
    fn weights_canonicalize_without_spaces() {
        let source = map(&[("MINER_WEIGHTS", " 70 , 30 ")]);
        let tuning = MiningTuning::from_source(&source).expect("spaced weights parse");
        assert_eq!(tuning.canonical_values()["MINER_WEIGHTS"], "70,30");
    }

    #[test]
    fn manual_fanout_below_minimum_is_rejected() {
        let source = map(&[
            ("SPAM_FANOUT_AUTO", "false"),
            ("SPAM_FANOUT_UTXOS", "5"),
            ("SPAM_FILL_BLOCK_RATIO", "2.0"),
        ]);
        let error = SpamTuning::from_source(&source).unwrap_err();
        assert!(error.to_string().contains("SPAM_FANOUT_UTXOS"));
    }

    #[test]
    fn manual_fanout_is_ignored_in_output_mode() {
        let source = map(&[
            ("SPAM_FANOUT_AUTO", "false"),
            ("SPAM_FANOUT_UTXOS", "5"),
            ("SPAM_TX_DATA_MAX_BYTES", "0"),
        ]);
        SpamTuning::from_source(&source).expect("OUTPUT mode skips the fanout minimum");
    }

    #[test]
    fn automatic_fanout_separates_minimum_from_headroom() {
        let (mut tuning, _) = SpamTuning::from_source(&map(&[])).expect("defaults are valid");
        tuning.fill_block_ratio = 5.0;
        assert_eq!(tuning.minimum_data_fanout(), 50);
        assert_eq!(tuning.desired_data_fanout(), 75);

        tuning.fanout_auto = false;
        tuning.fanout_utxos = 80;
        assert_eq!(tuning.minimum_data_fanout(), 50);
        assert_eq!(tuning.desired_data_fanout(), 80);
    }

    #[test]
    fn data_max_bytes_is_clamped_with_warning() {
        let source = map(&[("SPAM_TX_DATA_MAX_BYTES", "200000")]);
        let (tuning, warnings) = SpamTuning::from_source(&source).expect("clamped, not rejected");
        assert_eq!(tuning.data_max_bytes, MAX_DATA_BYTES);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("clamping"));
    }

    #[test]
    fn effective_data_min_never_exceeds_max() {
        let source = map(&[
            ("SPAM_TX_DATA_MAX_BYTES", "100"),
            ("SPAM_TX_DATA_MIN_BYTES", "250"),
        ]);
        let (tuning, _) = SpamTuning::from_source(&source).expect("valid");
        assert_eq!(tuning.data_min_bytes, 250);
        assert_eq!(tuning.effective_data_min_bytes(), 100);
    }

    #[test]
    fn legacy_txs_per_block_alias_is_honored() {
        let source = map(&[("SPAM_TXS_PER_BLOCK", "500")]);
        let (tuning, _) = SpamTuning::from_source(&source).expect("alias parses");
        assert_eq!(tuning.fixed_txs_per_block, 500);
    }

    #[test]
    fn legacy_per_miner_alias_converts_with_warning() {
        let source = map(&[("SPAM_PER_MINER_PER_BLOCK", "40")]);
        let (tuning, warnings) = SpamTuning::from_source(&source).expect("alias parses");
        assert_eq!(tuning.fixed_txs_per_block, 40 * MINER_COUNT);
        assert!(warnings[0].contains("deprecated"));
    }

    #[test]
    fn canonical_key_wins_over_legacy_alias() {
        let source = map(&[
            ("SPAM_FIXED_TXS_PER_BLOCK", "10"),
            ("SPAM_TXS_PER_BLOCK", "500"),
        ]);
        let (tuning, _) = SpamTuning::from_source(&source).expect("valid");
        assert_eq!(tuning.fixed_txs_per_block, 10);
    }

    #[test]
    fn negative_fee_is_rejected() {
        let source = map(&[("SPAM_FEE", "-0.1")]);
        let error = SpamTuning::from_source(&source).unwrap_err();
        assert!(error.to_string().contains("SPAM_FEE"));
    }

    #[test]
    fn errors_across_both_subsets_aggregate() {
        let source = map(&[
            ("BLOCK_INTERVAL_MEAN_SECS", "0"),
            ("SPAM_FEE", "not-a-number"),
        ]);
        let error = LiveTuning::from_source(&source).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("BLOCK_INTERVAL_MEAN_SECS"));
        assert!(message.contains("SPAM_FEE"));
    }

    #[test]
    fn staged_map_covers_the_whole_catalog() {
        let staged = staged_map(&map(&[("SPAM_FEE", "0.0002")]));
        assert_eq!(staged.len(), MANAGED_SETTINGS.len());
        assert_eq!(staged["SPAM_FEE"], "0.0002");
        assert_eq!(staged["SPAM_FLOOR_POOL_TXS"], "4000");
    }

    #[test]
    fn use_raw_tx_spam_is_pinned_to_the_raw_engine() {
        let (tuning, warnings) =
            SpamTuning::from_source(&map(&[])).expect("defaults must be valid");
        assert!(tuning.use_raw);
        assert!(warnings.is_empty());

        // A legacy explicit false is overridden with a warning, not an error.
        let (tuning, warnings) = SpamTuning::from_source(&map(&[("USE_RAW_TX_SPAM", "false")]))
            .expect("legacy false is tolerated");
        assert!(tuning.use_raw);
        assert!(warnings.iter().any(|w| w.contains("deprecated")));

        // No longer a managed key.
        assert!(!is_managed_key("USE_RAW_TX_SPAM"));
    }

    #[test]
    fn legacy_fallback_fee_seeds_spam_fee_with_a_warning() {
        let source = map(&[("FALLBACK_FEE", "0.0005")]);
        let (tuning, warnings) = SpamTuning::from_source(&source).expect("legacy env is valid");
        assert_eq!(tuning.spam_fee, 0.0005);
        assert!(warnings.iter().any(|w| w.contains("legacy FALLBACK_FEE")));

        // An explicit SPAM_FEE wins over the legacy variable, silently.
        let source = map(&[("FALLBACK_FEE", "0.0005"), ("SPAM_FEE", "0.0002")]);
        let (tuning, warnings) = SpamTuning::from_source(&source).expect("split env is valid");
        assert_eq!(tuning.spam_fee, 0.0002);
        assert!(warnings.is_empty());
    }

    #[test]
    fn catalog_defaults_parse_through_their_own_validators() {
        for spec in MANAGED_SETTINGS {
            let mut source = BTreeMap::new();
            source.insert(spec.key.to_string(), spec.default.to_string());
            LiveTuning::from_source(&source)
                .unwrap_or_else(|error| panic!("default for {} is invalid: {error}", spec.key));
        }
    }
}
