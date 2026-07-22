use serde::{Deserialize, Serialize};

pub const FAUCET_PRIORITY_DELTA_SATS: i64 = 10_000_000_000;
pub const FAUCET_MAX_OUTPUTS: usize = 100;
pub const FAUCET_MAX_TX_VBYTES: u64 = 10_000;
pub const FAUCET_PRIORITY_DOMINANCE_FACTOR: u64 = 100;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaucetSource {
    #[default]
    Auto,
    Node2,
    Node3,
}

impl FaucetSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Node2 => "node2",
            Self::Node3 => "node3",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaucetSourceNode {
    Node2,
    Node3,
}

impl FaucetSourceNode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node2 => "node2",
            Self::Node3 => "node3",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FaucetOutput {
    pub address: String,
    pub amount_sats: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FaucetJobRequest {
    #[serde(default)]
    pub source: FaucetSource,
    pub outputs: Vec<FaucetOutput>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaucetDeliveryState {
    Armed,
    Confirmed,
    Recovering,
    DeliveryFailed,
    AbortedAfterSubmission,
    OrphanedAfterConfirmation,
}

impl FaucetDeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Armed => "armed",
            Self::Confirmed => "confirmed",
            Self::Recovering => "recovering",
            Self::DeliveryFailed => "delivery_failed",
            Self::AbortedAfterSubmission => "aborted_after_submission",
            Self::OrphanedAfterConfirmation => "orphaned_after_confirmation",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FaucetTransfer {
    pub delivery_state: FaucetDeliveryState,
    pub txid: String,
    pub source: FaucetSourceNode,
    pub wallet_name: String,
    pub outputs: Vec<FaucetOutput>,
    pub total_sats: u64,
    pub change_sats: u64,
    pub actual_fee_sats: u64,
    pub priority_delta_sats: i64,
    pub vsize: u64,
    pub armed_nodes: Vec<String>,
    pub visibility: String,
    pub armed_at_height: u64,
    pub armed_at_block_hash: String,
    pub armed_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_block_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub observer_unconfirmed: bool,
    pub transfer_url: String,
    pub explorer_url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FaucetWalletStatus {
    pub source: FaucetSourceNode,
    pub wallet_name: String,
    pub eligible_confirmed_sats: u64,
    pub available_after_reserve_sats: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FaucetStatusResponse {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_probe_error: Option<String>,
    pub max_request_sats: u64,
    pub max_outputs: usize,
    pub wallet_reserve_sats: u64,
    pub max_tx_vbytes: u64,
    pub priority_delta_sats: i64,
    pub wallets: Vec<FaucetWalletStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_transfer: Option<FaucetTransfer>,
    pub recent_transfers: Vec<FaucetTransfer>,
}

pub type FaucetJobResult = FaucetTransfer;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_source_defaults_to_auto() {
        let request: FaucetJobRequest =
            serde_json::from_str(r#"{"outputs":[{"address":"bcrt1qexample","amount_sats":1}]}"#)
                .unwrap();
        assert_eq!(request.source, FaucetSource::Auto);
    }
}
