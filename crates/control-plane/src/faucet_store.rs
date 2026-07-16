//! Private, atomic faucet transfer ledger and recovery material.

use crate::faucet_job::FaucetInput;
use crate::storage;
use serde::{Deserialize, Serialize};
use simchain_common::control_api::{FaucetDeliveryState, FaucetTransfer};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FAUCET_SCHEMA_VERSION: u32 = 1;
const FILE_NAME: &str = "faucet-transfers.json";
const MAX_TERMINAL_HISTORY: usize = 100;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredFaucetTransfer {
    pub public: FaucetTransfer,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_tx_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_inputs: Vec<FaucetInput>,
}

impl StoredFaucetTransfer {
    fn is_pending(&self) -> bool {
        matches!(
            self.public.delivery_state,
            FaucetDeliveryState::Armed | FaucetDeliveryState::Recovering
        )
    }

    fn clear_recovery_material(&mut self) {
        self.raw_tx_hex = None;
        self.selected_inputs.clear();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedFaucetTransfers {
    schema_version: u32,
    transfers: Vec<StoredFaucetTransfer>,
}

impl Default for PersistedFaucetTransfers {
    fn default() -> Self {
        Self {
            schema_version: FAUCET_SCHEMA_VERSION,
            transfers: Vec::new(),
        }
    }
}

pub struct FaucetStore {
    path: PathBuf,
    state_dir: PathBuf,
    state: Mutex<PersistedFaucetTransfers>,
}

impl FaucetStore {
    pub fn open(state_dir: &Path) -> anyhow::Result<Self> {
        let path = state_dir.join(FILE_NAME);
        let persisted = match fs::read_to_string(&path) {
            Ok(content) => {
                serde_json::from_str::<PersistedFaucetTransfers>(&content).map_err(|error| {
                    anyhow::anyhow!("faucet store {} is corrupt: {error}", path.display())
                })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                PersistedFaucetTransfers::default()
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            persisted.schema_version == FAUCET_SCHEMA_VERSION,
            "unsupported faucet store schema {} (expected {FAUCET_SCHEMA_VERSION})",
            persisted.schema_version
        );
        anyhow::ensure!(
            persisted
                .transfers
                .iter()
                .filter(|transfer| transfer.is_pending())
                .count()
                <= 1,
            "faucet store contains more than one pending transfer"
        );
        Ok(Self {
            path,
            state_dir: state_dir.to_path_buf(),
            state: Mutex::new(persisted),
        })
    }

    pub fn pending(&self) -> Option<StoredFaucetTransfer> {
        self.state
            .lock()
            .expect("faucet store lock")
            .transfers
            .iter()
            .find(|transfer| transfer.is_pending())
            .cloned()
    }

    pub fn get(&self, txid: &str) -> Option<FaucetTransfer> {
        self.state
            .lock()
            .expect("faucet store lock")
            .transfers
            .iter()
            .find(|transfer| transfer.public.txid == txid)
            .map(|transfer| transfer.public.clone())
    }

    pub fn recent(&self) -> Vec<FaucetTransfer> {
        self.state
            .lock()
            .expect("faucet store lock")
            .transfers
            .iter()
            .rev()
            .map(|transfer| transfer.public.clone())
            .collect()
    }

    pub fn latest_confirmed(&self) -> Option<FaucetTransfer> {
        self.state
            .lock()
            .expect("faucet store lock")
            .transfers
            .iter()
            .rev()
            .find(|transfer| transfer.public.delivery_state == FaucetDeliveryState::Confirmed)
            .map(|transfer| transfer.public.clone())
    }

    pub fn arm(&self, transfer: StoredFaucetTransfer) -> anyhow::Result<()> {
        anyhow::ensure!(transfer.is_pending(), "armed faucet record must be pending");
        let mut state = self.state.lock().expect("faucet store lock");
        if let Some(existing) = state
            .transfers
            .iter_mut()
            .find(|existing| existing.public.txid == transfer.public.txid)
        {
            *existing = transfer;
        } else {
            anyhow::ensure!(
                !state.transfers.iter().any(StoredFaucetTransfer::is_pending),
                "another faucet transfer is pending"
            );
            state.transfers.push(transfer);
        }
        trim(&mut state.transfers);
        self.save_locked(&state)
    }

    pub fn mark_recovering(&self, txid: &str, error: Option<String>) -> anyhow::Result<()> {
        self.update(txid, |record| {
            record.public.delivery_state = FaucetDeliveryState::Recovering;
            record.public.last_error = error;
        })
    }

    pub fn mark_armed(&self, txid: &str) -> anyhow::Result<()> {
        self.update(txid, |record| {
            record.public.delivery_state = FaucetDeliveryState::Armed;
            record.public.last_error = None;
        })
    }

    pub fn mark_confirmed(
        &self,
        txid: &str,
        height: u64,
        block_hash: String,
        confirmed_at_ms: u64,
    ) -> anyhow::Result<()> {
        self.update(txid, |record| {
            record.public.delivery_state = FaucetDeliveryState::Confirmed;
            record.public.confirmed_height = Some(height);
            record.public.confirmed_block_hash = Some(block_hash);
            record.public.confirmed_at_ms = Some(confirmed_at_ms);
            record.public.last_error = None;
            record.clear_recovery_material();
        })
    }

    pub fn mark_failed(
        &self,
        txid: &str,
        state: FaucetDeliveryState,
        message: String,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                state,
                FaucetDeliveryState::DeliveryFailed | FaucetDeliveryState::AbortedAfterSubmission
            ),
            "invalid terminal faucet failure state"
        );
        self.update(txid, |record| {
            record.public.delivery_state = state;
            record.public.last_error = Some(message);
            record.clear_recovery_material();
        })
    }

    pub fn mark_orphaned(&self, txid: &str, message: String) -> anyhow::Result<()> {
        self.update(txid, |record| {
            record.public.delivery_state = FaucetDeliveryState::OrphanedAfterConfirmation;
            record.public.last_error = Some(message);
            record.clear_recovery_material();
        })
    }

    fn update(
        &self,
        txid: &str,
        update: impl FnOnce(&mut StoredFaucetTransfer),
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().expect("faucet store lock");
        let record = state
            .transfers
            .iter_mut()
            .find(|record| record.public.txid == txid)
            .ok_or_else(|| anyhow::anyhow!("faucet transfer {txid} is missing"))?;
        update(record);
        trim(&mut state.transfers);
        self.save_locked(&state)
    }

    fn save_locked(&self, persisted: &PersistedFaucetTransfers) -> anyhow::Result<()> {
        let mut content = serde_json::to_string_pretty(persisted)?;
        content.push('\n');
        let ownership = storage::dir_ownership(&self.state_dir, 0o600)?;
        Ok(storage::write_atomic(&self.path, &content, ownership)?)
    }
}

fn trim(transfers: &mut Vec<StoredFaucetTransfer>) {
    while transfers
        .iter()
        .filter(|transfer| !transfer.is_pending())
        .count()
        > MAX_TERMINAL_HISTORY
    {
        if let Some(index) = transfers.iter().position(|transfer| !transfer.is_pending()) {
            transfers.remove(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simchain_common::control_api::{FaucetOutput, FaucetSourceNode};
    use std::os::unix::fs::PermissionsExt;

    fn transfer(txid: &str, state: FaucetDeliveryState) -> StoredFaucetTransfer {
        StoredFaucetTransfer {
            public: FaucetTransfer {
                delivery_state: state,
                txid: txid.to_string(),
                source: FaucetSourceNode::Node2,
                wallet_name: "node2".to_string(),
                outputs: vec![FaucetOutput {
                    address: "bcrt1qexample".to_string(),
                    amount_sats: 1,
                }],
                total_sats: 1,
                change_sats: 1,
                actual_fee_sats: 0,
                priority_delta_sats: 10_000_000_000,
                vsize: 100,
                armed_nodes: vec!["node2".into(), "node3".into()],
                visibility: "miner_only_unconfirmed".to_string(),
                armed_at_height: 204,
                armed_at_block_hash: "block".to_string(),
                armed_at_ms: 1,
                confirmed_height: None,
                confirmed_block_hash: None,
                confirmed_at_ms: None,
                last_error: None,
                observer_unconfirmed: true,
                transfer_url: format!("/api/v1/faucet/transfers/{txid}"),
                explorer_url: format!("http://explorer/tx/{txid}"),
            },
            raw_tx_hex: Some("00".to_string()),
            selected_inputs: Vec::new(),
        }
    }

    #[test]
    fn pending_survives_history_pruning_and_terminal_clears_private_material() {
        let dir = tempfile::tempdir().unwrap();
        let store = FaucetStore::open(dir.path()).unwrap();
        for index in 0..105 {
            let mut item = transfer(&format!("tx-{index}"), FaucetDeliveryState::Armed);
            item.public.delivery_state = FaucetDeliveryState::Confirmed;
            let mut state = store.state.lock().unwrap();
            state.transfers.push(item);
            trim(&mut state.transfers);
            store.save_locked(&state).unwrap();
        }
        store
            .arm(transfer("pending", FaucetDeliveryState::Armed))
            .unwrap();
        assert_eq!(store.pending().unwrap().public.txid, "pending");
        assert_eq!(store.recent().len(), 101);
        store
            .mark_confirmed("pending", 205, "confirmed".into(), 2)
            .unwrap();
        assert!(store.pending().is_none());
        assert_eq!(store.latest_confirmed().unwrap().txid, "pending");
        store
            .mark_orphaned("pending", "orphaned by test reorg".to_string())
            .unwrap();
        assert_eq!(
            store.get("pending").unwrap().delivery_state,
            FaucetDeliveryState::OrphanedAfterConfirmation
        );
        assert!(store
            .state
            .lock()
            .unwrap()
            .transfers
            .iter()
            .find(|record| record.public.txid == "pending")
            .unwrap()
            .raw_tx_hex
            .is_none());
        assert_eq!(
            fs::metadata(dir.path().join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn corrupt_and_future_state_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE_NAME), "not-json").unwrap();
        assert!(FaucetStore::open(dir.path()).is_err());
        fs::write(
            dir.path().join(FILE_NAME),
            r#"{"schema_version":2,"transfers":[]}"#,
        )
        .unwrap();
        assert!(FaucetStore::open(dir.path()).is_err());
    }
}
