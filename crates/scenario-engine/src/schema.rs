use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use simchain_common::control_api::FaucetSource;
use simchain_common::internal_api::DesiredState;
use std::collections::{BTreeMap, HashSet};
use std::{fmt, fs, path::Path};

pub const BOOTSTRAP_HEIGHT: u64 = 204;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub version: u64,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    WaitHeight {
        height: u64,
    },
    WaitUntil {
        condition: WaitCondition,
        #[serde(default = "default_wait_until_timeout_secs")]
        timeout_secs: u64,
    },
    WaitTx {
        #[serde(flatten)]
        wait: WaitTxStep,
    },
    AssertHeight {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        equals: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_least: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_most: Option<u64>,
    },
    AssertComponent {
        #[serde(flatten)]
        expected: ComponentExpectation,
    },
    Sleep {
        secs: u64,
    },
    PauseMining,
    ResumeMining,
    Mine {
        node: MinerNode,
        blocks: u64,
    },
    Reorg {
        depth: u64,
        #[serde(default)]
        empty: bool,
        #[serde(default = "default_reorg_node")]
        node: MinerNode,
        #[serde(default)]
        adds_new_txs: u64,
        #[serde(default)]
        double_spend_pct: u8,
    },
    SpamBurst {
        node: MinerNode,
        txs: u64,
        outputs_per_tx: u64,
    },
    SetConfig {
        #[serde(deserialize_with = "deserialize_settings")]
        settings: BTreeMap<String, String>,
    },
    AssertConfig {
        #[serde(deserialize_with = "deserialize_settings")]
        settings: BTreeMap<String, String>,
        #[serde(default = "default_assert_effective")]
        effective: bool,
    },
    Faucet {
        #[serde(default)]
        source: FaucetSource,
        outputs: Vec<FaucetScenarioOutput>,
        #[serde(default = "default_wait_confirmed")]
        wait_confirmed: bool,
        #[serde(default = "default_faucet_timeout_secs")]
        timeout_secs: u64,
    },
    Partition {
        node: MinerNode,
        main_blocks: u64,
        isolated_blocks: u64,
    },
    Degrade {
        node: NetworkNode,
        delay_ms: u64,
        #[serde(default)]
        loss_pct: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until_height: Option<u64>,
    },
    Checkpoint {
        #[serde(flatten)]
        checkpoint: CheckpointStep,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointStep {
    pub name: String,
    #[serde(default = "default_checkpoint_pause")]
    pub pause: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

fn default_checkpoint_pause() -> bool {
    true
}

fn default_assert_effective() -> bool {
    true
}

fn default_wait_confirmed() -> bool {
    true
}

fn default_faucet_timeout_secs() -> u64 {
    900
}

fn default_wait_until_timeout_secs() -> u64 {
    900
}

fn default_reorg_node() -> MinerNode {
    MinerNode::Node3
}

impl Step {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WaitHeight { .. } => "wait_height",
            Self::WaitUntil { .. } => "wait_until",
            Self::WaitTx { .. } => "wait_tx",
            Self::AssertHeight { .. } => "assert_height",
            Self::AssertComponent { .. } => "assert_component",
            Self::Sleep { .. } => "sleep",
            Self::PauseMining => "pause_mining",
            Self::ResumeMining => "resume_mining",
            Self::Mine { .. } => "mine",
            Self::Reorg { .. } => "reorg",
            Self::SpamBurst { .. } => "spam_burst",
            Self::SetConfig { .. } => "set_config",
            Self::AssertConfig { .. } => "assert_config",
            Self::Faucet { .. } => "faucet",
            Self::Partition { .. } => "partition",
            Self::Degrade { .. } => "degrade",
            Self::Checkpoint { .. } => "checkpoint",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TxWaitState {
    Seen,
    Mempool,
    Confirmed,
    Missing,
}

impl TxWaitState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seen => "seen",
            Self::Mempool => "mempool",
            Self::Confirmed => "confirmed",
            Self::Missing => "missing",
        }
    }
}

fn default_tx_wait_state() -> TxWaitState {
    TxWaitState::Confirmed
}

fn default_wait_tx_timeout_secs() -> u64 {
    900
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaitTxStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txid_env: Option<String>,
    #[serde(default = "default_tx_wait_state")]
    pub state: TxWaitState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmations: Option<u64>,
    #[serde(default = "default_wait_tx_timeout_secs")]
    pub timeout_secs: u64,
}

impl WaitTxStep {
    pub fn expected_confirmations(&self) -> u64 {
        match self.state {
            TxWaitState::Confirmed => self.confirmations.unwrap_or(1),
            TxWaitState::Seen | TxWaitState::Mempool | TxWaitState::Missing => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MinerNode {
    #[serde(rename = "btc-simnet-node2", alias = "node2")]
    Node2,
    #[serde(rename = "btc-simnet-node3", alias = "node3")]
    Node3,
}

impl MinerNode {
    pub fn short_name(self) -> &'static str {
        match self {
            Self::Node2 => "node2",
            Self::Node3 => "node3",
        }
    }
}

impl fmt::Display for MinerNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Node2 => "btc-simnet-node2",
            Self::Node3 => "btc-simnet-node3",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScenarioComponent {
    Mining,
    Spam,
    NetworkAgentNode1,
    NetworkAgentNode2,
    NetworkAgentNode3,
}

impl ScenarioComponent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mining => "mining",
            Self::Spam => "spam",
            Self::NetworkAgentNode1 => "network-agent-node1",
            Self::NetworkAgentNode2 => "network-agent-node2",
            Self::NetworkAgentNode3 => "network-agent-node3",
        }
    }

    pub fn network_node(self) -> Option<&'static str> {
        match self {
            Self::NetworkAgentNode1 => Some("node1"),
            Self::NetworkAgentNode2 => Some("node2"),
            Self::NetworkAgentNode3 => Some("node3"),
            Self::Mining | Self::Spam => None,
        }
    }
}

impl fmt::Display for ScenarioComponent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentExpectation {
    pub component: ScenarioComponent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<DesiredState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_state: Option<DesiredState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_height_at_least: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_lease_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_phase: Option<String>,
}

impl ComponentExpectation {
    pub fn has_assertions(&self) -> bool {
        self.reachable.is_some()
            || self.status.is_some()
            || self.phase.is_some()
            || self.desired_state.is_some()
            || self.effective_state.is_some()
            || self.effective_generation.is_some()
            || self.observed_height_at_least.is_some()
            || self.active_lease_count.is_some()
            || self.cycle_phase.is_some()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitCondition {
    HeightAtLeast {
        height: u64,
    },
    MempoolTxsAtLeast {
        count: usize,
    },
    MempoolTxsAtMost {
        count: usize,
    },
    Component {
        #[serde(flatten)]
        expected: ComponentExpectation,
    },
}

impl WaitCondition {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::HeightAtLeast { .. } => "height_at_least",
            Self::MempoolTxsAtLeast { .. } => "mempool_txs_at_least",
            Self::MempoolTxsAtMost { .. } => "mempool_txs_at_most",
            Self::Component { .. } => "component",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NetworkNode {
    #[serde(rename = "node1", alias = "btc-simnet-node1")]
    Node1,
    #[serde(rename = "node2", alias = "btc-simnet-node2")]
    Node2,
    #[serde(rename = "node3", alias = "btc-simnet-node3")]
    Node3,
}

impl NetworkNode {
    pub fn short_name(self) -> &'static str {
        match self {
            Self::Node1 => "node1",
            Self::Node2 => "node2",
            Self::Node3 => "node3",
        }
    }
}

impl fmt::Display for NetworkNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.short_name())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FaucetScenarioOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address_env: Option<String>,
    pub amount: String,
}

impl FaucetScenarioOutput {
    pub fn amount_sats(&self) -> Result<u64> {
        parse_scenario_amount_sats(&self.amount)
    }
}

pub fn parse_scenario_amount_sats(value: &str) -> Result<u64> {
    let trimmed = value.trim();
    if let Some(sats) = trimmed.strip_suffix("sat") {
        return sats
            .trim()
            .parse::<u64>()
            .context("satoshi amount must be an unsigned integer");
    }
    let btc = trimmed.strip_suffix("btc").unwrap_or(trimmed);
    simchain_common::parse_btc_sats(btc).map_err(anyhow::Error::msg)
}

impl Scenario {
    pub fn parse(contents: &str) -> Result<Self> {
        let scenario: Self =
            serde_yaml::from_str(contents).context("failed to parse scenario YAML")?;
        scenario.validate()?;
        Ok(scenario)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read scenario file {}", path.display()))?;
        Self::parse(&contents).with_context(|| format!("invalid scenario file {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported scenario version {}; expected 1", self.version);
        }
        let mut checkpoint_names = HashSet::new();
        for (index, step) in self.steps.iter().enumerate() {
            let error = match step {
                Step::WaitHeight { height } if *height < BOOTSTRAP_HEIGHT => {
                    Some(format!("height must be at least {BOOTSTRAP_HEIGHT}"))
                }
                Step::WaitUntil { timeout_secs, .. } if *timeout_secs == 0 => {
                    Some("timeout_secs must be positive".to_string())
                }
                Step::WaitUntil { condition, .. } => validate_wait_condition(condition),
                Step::WaitTx { wait } => validate_wait_tx(wait),
                Step::AssertHeight {
                    equals,
                    at_least,
                    at_most,
                } => validate_height_assertion(*equals, *at_least, *at_most),
                Step::AssertComponent { expected } if !expected.has_assertions() => {
                    Some("at least one component expectation must be set".to_string())
                }
                Step::Sleep { secs } if *secs == 0 => Some("secs must be positive".to_string()),
                Step::Mine { blocks, .. } if *blocks == 0 => {
                    Some("blocks must be positive".to_string())
                }
                Step::Reorg { depth, .. } if *depth == 0 => {
                    Some("depth must be positive".to_string())
                }
                Step::Reorg { depth, .. } if *depth > 100 => {
                    Some("depth must not exceed 100".to_string())
                }
                Step::Reorg { adds_new_txs, .. } if *adds_new_txs > 10_000 => {
                    Some("adds_new_txs must not exceed 10000".to_string())
                }
                Step::Reorg {
                    double_spend_pct, ..
                } if *double_spend_pct > 100 => {
                    Some("double_spend_pct must be between 0 and 100".to_string())
                }
                Step::SpamBurst { txs, .. } if *txs == 0 => {
                    Some("txs must be positive".to_string())
                }
                Step::SetConfig { settings } if settings.is_empty() => {
                    Some("settings must not be empty".to_string())
                }
                Step::SetConfig { settings } | Step::AssertConfig { settings, .. }
                    if settings.keys().any(|key| key.trim().is_empty()) =>
                {
                    Some("setting keys must not be empty".to_string())
                }
                Step::AssertConfig { settings, .. } if settings.is_empty() => {
                    Some("settings must not be empty".to_string())
                }
                Step::Faucet { outputs, .. } if outputs.is_empty() => {
                    Some("outputs must not be empty".to_string())
                }
                Step::Faucet { outputs, .. } if outputs.len() > 100 => {
                    Some("outputs must not exceed 100 entries".to_string())
                }
                Step::Faucet { timeout_secs, .. } if *timeout_secs == 0 => {
                    Some("timeout_secs must be positive".to_string())
                }
                Step::Faucet { outputs, .. } => outputs
                    .iter()
                    .find_map(validate_faucet_output)
                    .map(|error| format!("invalid faucet output: {error}")),
                Step::Partition {
                    main_blocks,
                    isolated_blocks,
                    ..
                } if *main_blocks == 0 || *isolated_blocks == 0 => {
                    Some("main_blocks and isolated_blocks must be positive".to_string())
                }
                Step::Partition {
                    main_blocks,
                    isolated_blocks,
                    ..
                } if main_blocks == isolated_blocks => {
                    Some("main_blocks and isolated_blocks must differ".to_string())
                }
                Step::Degrade {
                    delay_ms, loss_pct, ..
                } if *delay_ms == 0 && *loss_pct == 0.0 => {
                    Some("delay_ms or loss_pct must be positive".to_string())
                }
                Step::Degrade { delay_ms, .. } if *delay_ms > 600_000 => {
                    Some("delay_ms must not exceed 600000".to_string())
                }
                Step::Degrade { loss_pct, .. }
                    if !loss_pct.is_finite() || !(0.0..=100.0).contains(loss_pct) =>
                {
                    Some("loss_pct must be a finite number from 0 through 100".to_string())
                }
                Step::Degrade {
                    seconds,
                    until_height,
                    ..
                } => validate_degrade_duration(*seconds, *until_height),
                Step::Checkpoint { checkpoint }
                    if checkpoint.name.is_empty()
                        || !checkpoint.name.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'-' | b'_' | b'.' | b'~')
                        }) =>
                {
                    Some("checkpoint name must be non-empty and URL-safe".to_string())
                }
                Step::Checkpoint { checkpoint } if checkpoint.name.len() > 100 => {
                    Some("checkpoint name must not exceed 100 bytes".to_string())
                }
                Step::Checkpoint { checkpoint }
                    if checkpoint.pause && checkpoint.timeout_secs.is_none() =>
                {
                    Some("timeout_secs is required when pause is true".to_string())
                }
                Step::Checkpoint { checkpoint } if checkpoint.timeout_secs == Some(0) => {
                    Some("timeout_secs must be positive".to_string())
                }
                Step::Checkpoint { checkpoint }
                    if !checkpoint_names.insert(checkpoint.name.clone()) =>
                {
                    Some("checkpoint names must be unique".to_string())
                }
                _ => None,
            };
            if let Some(error) = error {
                bail!("invalid step {} ({}): {error}", index + 1, step.kind());
            }
        }
        Ok(())
    }

    pub fn resolve_env_addresses(&mut self) -> Result<()> {
        for step in &mut self.steps {
            let Step::Faucet { outputs, .. } = step else {
                continue;
            };
            for output in outputs {
                let Some(env) = output.address_env.take() else {
                    continue;
                };
                let address = std::env::var(&env)
                    .with_context(|| format!("environment variable {env} is not set"))?;
                let address = address.trim().to_string();
                if address.is_empty() {
                    bail!("environment variable {env} is empty");
                }
                output.address = Some(address);
            }
        }
        self.validate()
    }

    pub fn resolve_env_values(&mut self) -> Result<()> {
        for step in &mut self.steps {
            match step {
                Step::Faucet { outputs, .. } => {
                    for output in outputs {
                        let Some(env) = output.address_env.take() else {
                            continue;
                        };
                        let address = std::env::var(&env)
                            .with_context(|| format!("environment variable {env} is not set"))?;
                        let address = address.trim().to_string();
                        if address.is_empty() {
                            bail!("environment variable {env} is empty");
                        }
                        output.address = Some(address);
                    }
                }
                Step::WaitTx { wait } => {
                    let Some(env) = wait.txid_env.take() else {
                        continue;
                    };
                    let txid = std::env::var(&env)
                        .with_context(|| format!("environment variable {env} is not set"))?;
                    let txid = txid.trim().to_string();
                    if txid.is_empty() {
                        bail!("environment variable {env} is empty");
                    }
                    wait.txid = Some(txid);
                }
                _ => {}
            }
        }
        self.validate()
    }

    pub fn resolve_env_values_yaml(contents: &str) -> Result<String> {
        let mut scenario = Self::parse(contents)?;
        scenario.resolve_env_values()?;
        serde_yaml::to_string(&scenario).context("failed to serialize resolved scenario")
    }

    pub fn resolve_env_addresses_yaml(contents: &str) -> Result<String> {
        let mut scenario = Self::parse(contents)?;
        scenario.resolve_env_values()?;
        serde_yaml::to_string(&scenario).context("failed to serialize resolved scenario")
    }
}

fn validate_wait_tx(wait: &WaitTxStep) -> Option<String> {
    let has_txid = wait
        .txid
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_env = wait
        .txid_env
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if has_txid == has_env {
        return Some("exactly one of txid or txid_env is required".to_string());
    }
    if let Some(txid) = wait.txid.as_deref() {
        let txid = txid.trim();
        if txid.len() != 64 || !txid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Some("txid must be 64 hexadecimal characters".to_string());
        }
    }
    if let Some(env) = wait.txid_env.as_deref() {
        if !env
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_'))
        {
            return Some("txid_env must be a simple environment variable name".to_string());
        }
    }
    if wait.timeout_secs == 0 {
        return Some("timeout_secs must be positive".to_string());
    }
    match (wait.state, wait.confirmations) {
        (TxWaitState::Confirmed, Some(0)) => Some("confirmations must be positive".to_string()),
        (TxWaitState::Seen | TxWaitState::Mempool | TxWaitState::Missing, Some(_)) => {
            Some("confirmations can only be used with state: confirmed".to_string())
        }
        _ => None,
    }
}

fn validate_wait_condition(condition: &WaitCondition) -> Option<String> {
    match condition {
        WaitCondition::HeightAtLeast { height } if *height < BOOTSTRAP_HEIGHT => {
            Some(format!("height must be at least {BOOTSTRAP_HEIGHT}"))
        }
        WaitCondition::MempoolTxsAtLeast { .. } | WaitCondition::MempoolTxsAtMost { .. } => None,
        WaitCondition::Component { expected } if !expected.has_assertions() => {
            Some("component wait requires at least one expectation".to_string())
        }
        WaitCondition::HeightAtLeast { .. } | WaitCondition::Component { .. } => None,
    }
}

fn validate_height_assertion(
    equals: Option<u64>,
    at_least: Option<u64>,
    at_most: Option<u64>,
) -> Option<String> {
    if equals.is_none() && at_least.is_none() && at_most.is_none() {
        return Some("at least one height condition must be set".to_string());
    }
    if equals.is_some() && (at_least.is_some() || at_most.is_some()) {
        return Some("equals cannot be combined with at_least or at_most".to_string());
    }
    match (at_least, at_most) {
        (Some(min), Some(max)) if min > max => {
            Some("at_least must be less than or equal to at_most".to_string())
        }
        _ => None,
    }
}

fn validate_degrade_duration(seconds: Option<u64>, until_height: Option<u64>) -> Option<String> {
    match (seconds, until_height) {
        (None, None) => Some("seconds or until_height is required".to_string()),
        (Some(_), Some(_)) => Some("seconds and until_height are mutually exclusive".to_string()),
        (Some(seconds), None) if seconds == 0 || seconds > 86_400 => {
            Some("seconds must be between 1 and 86400".to_string())
        }
        (None, Some(height)) if height < BOOTSTRAP_HEIGHT => {
            Some(format!("until_height must be at least {BOOTSTRAP_HEIGHT}"))
        }
        _ => None,
    }
}

fn validate_faucet_output(output: &FaucetScenarioOutput) -> Option<String> {
    let has_address = output
        .address
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_env = output
        .address_env
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if has_address == has_env {
        return Some("exactly one of address or address_env is required".to_string());
    }
    if let Some(env) = output.address_env.as_deref() {
        if !env
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_'))
        {
            return Some("address_env must be a simple environment variable name".to_string());
        }
    }
    match output.amount_sats() {
        Ok(0) => Some("amount must be positive".to_string()),
        Ok(_) => None,
        Err(error) => Some(format!("invalid amount '{}': {error}", output.amount)),
    }
}

fn deserialize_settings<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = BTreeMap::<String, serde_yaml::Value>::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(key, value)| scalar_to_string(value).map(|value| (key, value)))
        .collect()
}

fn scalar_to_string<E>(value: serde_yaml::Value) -> std::result::Result<String, E>
where
    E: serde::de::Error,
{
    match value {
        serde_yaml::Value::Null => Ok(String::new()),
        serde_yaml::Value::Bool(value) => Ok(value.to_string()),
        serde_yaml::Value::Number(value) => Ok(value.to_string()),
        serde_yaml::Value::String(value) => Ok(value),
        _ => Err(E::custom(
            "config values must be scalar strings, numbers, booleans, or null",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<Scenario> {
        Scenario::parse(yaml)
    }

    #[test]
    fn parses_valid_v1_and_preserves_order() {
        let scenario = parse(
            r#"
version: 1
steps:
  - type: wait_height
    height: 260
  - type: wait_until
    timeout_secs: 120
    condition:
      kind: component
      component: spam
      status: active
  - type: wait_tx
    txid: "1111111111111111111111111111111111111111111111111111111111111111"
    confirmations: 2
    timeout_secs: 120
  - type: assert_height
    at_least: 260
  - type: assert_component
    component: mining
    reachable: true
    desired_state: running
  - type: pause_mining
  - type: mine
    node: node2
    blocks: 3
  - type: reorg
    depth: 2
    empty: false
    node: node3
    adds_new_txs: 2
    double_spend_pct: 10
"#,
        )
        .unwrap();
        let kinds: Vec<_> = scenario.steps.iter().map(Step::kind).collect();
        assert_eq!(
            kinds,
            [
                "wait_height",
                "wait_until",
                "wait_tx",
                "assert_height",
                "assert_component",
                "pause_mining",
                "mine",
                "reorg"
            ]
        );
    }

    #[test]
    fn parses_wait_tx_and_resolves_txid_env() {
        let scenario = parse(
            r#"
version: 1
steps:
  - type: wait_tx
    txid_env: SIMCHAIN_TEST_TXID
    state: mempool
    timeout_secs: 60
"#,
        )
        .expect("valid wait_tx");
        let Step::WaitTx { wait } = &scenario.steps[0] else {
            panic!("wait_tx step");
        };
        assert_eq!(wait.state, TxWaitState::Mempool);
        assert_eq!(wait.expected_confirmations(), 0);

        std::env::set_var(
            "SIMCHAIN_TEST_TXID",
            "2222222222222222222222222222222222222222222222222222222222222222",
        );
        let resolved = Scenario::resolve_env_values_yaml(
            r#"
version: 1
steps:
  - type: wait_tx
    txid_env: SIMCHAIN_TEST_TXID
"#,
        )
        .expect("resolved wait_tx");
        let resolved_scenario = Scenario::parse(&resolved).expect("resolved scenario parses");
        let Step::WaitTx { wait } = &resolved_scenario.steps[0] else {
            panic!("resolved wait_tx step");
        };
        assert_eq!(
            wait.txid.as_deref(),
            Some("2222222222222222222222222222222222222222222222222222222222222222")
        );
        assert!(wait.txid_env.is_none());
        assert!(!resolved.contains("txid_env"));
        std::env::remove_var("SIMCHAIN_TEST_TXID");
    }

    #[test]
    fn checkpoints_default_to_pausing_and_require_unique_safe_names() {
        let scenario = parse(
            r#"
version: 1
steps:
  - type: checkpoint
    name: mempool_loaded
    timeout_secs: 60
  - type: checkpoint
    name: observed
    pause: false
"#,
        )
        .expect("valid checkpoints");
        let Step::Checkpoint { checkpoint } = &scenario.steps[0] else {
            panic!("checkpoint step");
        };
        assert!(checkpoint.pause);

        for yaml in [
            "version: 1\nsteps:\n  - type: checkpoint\n    name: bad/name\n    timeout_secs: 1\n",
            "version: 1\nsteps:\n  - type: checkpoint\n    name: held\n",
            "version: 1\nsteps:\n  - type: checkpoint\n    name: same\n    timeout_secs: 1\n  - type: checkpoint\n    name: same\n    timeout_secs: 1\n",
        ] {
            assert!(parse(yaml).is_err());
        }
    }

    #[test]
    fn parses_hot_control_steps_and_scalar_config_values() {
        let scenario = parse(
            r#"
version: 1
steps:
  - type: set_config
    settings:
      BLOCK_INTERVAL_MODE: fixed
      BLOCK_INTERVAL_MEAN_SECS: 10
      ENABLE_SPAM: true
      MINER_WEIGHTS:
  - type: assert_config
    effective: true
    settings:
      BLOCK_INTERVAL_MODE: fixed
      ENABLE_SPAM: true
  - type: faucet
    source: auto
    outputs:
      - address_env: FUND_ADD_1
        amount: 1btc
      - address: bcrt1qexample
        amount: 25000000sat
  - type: degrade
    node: node2
    delay_ms: 500
    loss_pct: 1
    seconds: 60
  - type: degrade
    node: node2
    delay_ms: 500
    until_height: 260
"#,
        )
        .expect("valid hot control steps");
        let Step::SetConfig { settings } = &scenario.steps[0] else {
            panic!("set_config step");
        };
        assert_eq!(settings["BLOCK_INTERVAL_MEAN_SECS"], "10");
        assert_eq!(settings["ENABLE_SPAM"], "true");
        assert_eq!(settings["MINER_WEIGHTS"], "");
        let Step::Faucet { outputs, .. } = &scenario.steps[2] else {
            panic!("faucet step");
        };
        assert_eq!(outputs[0].amount_sats().unwrap(), 100_000_000);
        assert_eq!(outputs[1].amount_sats().unwrap(), 25_000_000);
    }

    #[test]
    fn rejects_invalid_hot_control_steps() {
        for yaml in [
            "version: 1\nsteps:\n  - type: set_config\n    settings: {}\n",
            "version: 1\nsteps:\n  - type: assert_config\n    settings: {}\n",
            "version: 1\nsteps:\n  - type: faucet\n    outputs: []\n",
            "version: 1\nsteps:\n  - type: faucet\n    timeout_secs: 0\n    outputs:\n      - address_env: FUND/ADD\n        amount: 1btc\n",
            "version: 1\nsteps:\n  - type: faucet\n    outputs:\n      - address: a\n        address_env: FUND_ADD_1\n        amount: 1btc\n",
            "version: 1\nsteps:\n  - type: faucet\n    outputs:\n      - address_env: FUND_ADD_1\n        amount: 0sat\n",
            "version: 1\nsteps:\n  - type: degrade\n    node: node2\n    seconds: 60\n",
            "version: 1\nsteps:\n  - type: degrade\n    node: node2\n    delay_ms: 1\n    loss_pct: 101\n    seconds: 60\n",
            "version: 1\nsteps:\n  - type: degrade\n    node: node2\n    delay_ms: 1\n",
            "version: 1\nsteps:\n  - type: degrade\n    node: node2\n    delay_ms: 1\n    seconds: 1\n    until_height: 260\n",
            "version: 1\nsteps:\n  - type: wait_until\n    condition:\n      kind: component\n      component: spam\n",
            "version: 1\nsteps:\n  - type: wait_tx\n    timeout_secs: 1\n",
            "version: 1\nsteps:\n  - type: wait_tx\n    txid: abc\n",
            "version: 1\nsteps:\n  - type: wait_tx\n    txid_env: TX/ID\n",
            "version: 1\nsteps:\n  - type: wait_tx\n    txid: \"1111111111111111111111111111111111111111111111111111111111111111\"\n    txid_env: TARGET_TXID\n",
            "version: 1\nsteps:\n  - type: wait_tx\n    txid: \"1111111111111111111111111111111111111111111111111111111111111111\"\n    timeout_secs: 0\n",
            "version: 1\nsteps:\n  - type: wait_tx\n    txid: \"1111111111111111111111111111111111111111111111111111111111111111\"\n    confirmations: 0\n",
            "version: 1\nsteps:\n  - type: wait_tx\n    txid: \"1111111111111111111111111111111111111111111111111111111111111111\"\n    state: mempool\n    confirmations: 1\n",
            "version: 1\nsteps:\n  - type: assert_height\n",
            "version: 1\nsteps:\n  - type: assert_height\n    equals: 210\n    at_least: 204\n",
            "version: 1\nsteps:\n  - type: assert_component\n    component: spam\n",
            "version: 1\nsteps:\n  - type: reorg\n    depth: 101\n",
            "version: 1\nsteps:\n  - type: reorg\n    depth: 1\n    double_spend_pct: 101\n",
        ] {
            assert!(parse(yaml).is_err(), "{yaml}");
        }
    }

    #[test]
    fn resolves_faucet_address_env_before_upload() {
        std::env::set_var("SIMCHAIN_TEST_FUND_ADDRESS", "bcrt1qresolved");
        let yaml = r#"
version: 1
steps:
  - type: faucet
    outputs:
      - address_env: SIMCHAIN_TEST_FUND_ADDRESS
        amount: 1btc
"#;
        let resolved = Scenario::resolve_env_addresses_yaml(yaml).expect("resolved scenario");
        assert!(resolved.contains("address: bcrt1qresolved"));
        assert!(!resolved.contains("address_env"));
        std::env::remove_var("SIMCHAIN_TEST_FUND_ADDRESS");
    }

    #[test]
    fn missing_faucet_address_env_is_an_error() {
        std::env::remove_var("SIMCHAIN_TEST_MISSING_FUND_ADDRESS");
        let yaml = r#"
version: 1
steps:
  - type: faucet
    outputs:
      - address_env: SIMCHAIN_TEST_MISSING_FUND_ADDRESS
        amount: 1btc
"#;
        assert!(Scenario::resolve_env_addresses_yaml(yaml).is_err());
    }

    #[test]
    fn rejects_unknown_version_and_invalid_fields() {
        assert!(parse("version: 2\nsteps: []\n").is_err());
        assert!(parse("version: 1\nsteps:\n  - type: sleep\n    secs: 0\n").is_err());
        assert!(parse("version: 1\nsteps:\n  - type: wait_height\n    height: 203\n").is_err());
    }

    #[test]
    fn rejects_equal_partition_block_counts_and_unknown_miner() {
        assert!(parse(
            "version: 1\nsteps:\n  - type: partition\n    node: btc-simnet-node3\n    main_blocks: 4\n    isolated_blocks: 4\n",
        )
        .is_err());
        assert!(serde_yaml::from_str::<Scenario>(
            "version: 1\nsteps:\n  - type: mine\n    node: btc-simnet-node1\n    blocks: 1\n",
        )
        .is_err());
    }

    #[test]
    fn shipped_scenarios_are_valid() {
        for yaml in [
            include_str!("../../../scenarios/pause-then-burst.yml"),
            include_str!("../../../scenarios/reorg-during-sync.yml"),
            include_str!("../../../scenarios/partition-node3.yml"),
            include_str!("../../../scenarios/ci-checkpoint.yml"),
            include_str!("../../../scenarios/tutorial-one-block.yml"),
            include_str!("../../../scenarios/fresh-chain-tour.yml"),
            include_str!("../../../scenarios/all-features-live.yml"),
        ] {
            parse(yaml).unwrap();
        }
    }
}
