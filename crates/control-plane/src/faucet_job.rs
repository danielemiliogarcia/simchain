//! Bitcoin Core adapter and exact-zero faucet transaction primitives.

use crate::state::ControlPlaneConfig;
use bitcoincore_rpc::bitcoin::{
    absolute::LockTime,
    address::NetworkUnchecked,
    consensus::encode::{deserialize_hex, serialize_hex},
    transaction::Version,
    Address, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
};
use bitcoincore_rpc::{Client, RpcApi};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use simchain_common::control_api::{FaucetOutput, FaucetSourceNode, FAUCET_MAX_TX_VBYTES};
use std::collections::HashSet;
use std::str::FromStr;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FaucetInput {
    pub txid: String,
    pub vout: u32,
    pub amount_sats: u64,
    pub confirmations: u32,
}

#[derive(Deserialize)]
struct ListUnspentEntry {
    txid: Txid,
    vout: u32,
    #[serde(with = "bitcoincore_rpc::bitcoin::amount::serde::as_btc")]
    amount: Amount,
    confirmations: u32,
    spendable: bool,
    safe: bool,
}

impl FaucetInput {
    fn outpoint(&self) -> anyhow::Result<OutPoint> {
        Ok(OutPoint::new(Txid::from_str(&self.txid)?, self.vout))
    }
}

#[derive(Clone, Debug)]
pub struct FaucetPreflight {
    pub height: u64,
    pub best_hash: String,
    pub node2_inputs: Vec<FaucetInput>,
    pub node3_inputs: Vec<FaucetInput>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedFaucetTransaction {
    pub raw_tx_hex: String,
    pub txid: String,
    pub input_sats: u64,
    pub change_sats: u64,
    pub vsize: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriorityUpdate {
    pub previous_delta_sats: i64,
    pub desired_delta_sats: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MinerVerification {
    pub base_fee_sats: u64,
    pub modified_fee_sats: u64,
    pub fee_delta_sats: i64,
    pub vsize: u64,
    pub weight: Option<u64>,
    pub ancestor_count: u64,
    pub greatest_competing_feerate_sat_vb: u64,
    pub minimum_feerate_sat_vb: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaucetConfirmation {
    pub height: u64,
    pub block_hash: String,
}

pub trait FaucetBackend: Send + Sync {
    fn preflight(&self) -> anyhow::Result<FaucetPreflight>;
    fn lock_inputs(&self, source: FaucetSourceNode, inputs: &[FaucetInput]) -> anyhow::Result<()>;
    fn unlock_inputs(&self, source: FaucetSourceNode, inputs: &[FaucetInput])
        -> anyhow::Result<()>;
    fn prepare_transaction(
        &self,
        source: FaucetSourceNode,
        inputs: &[FaucetInput],
        outputs: &[FaucetOutput],
    ) -> anyhow::Result<PreparedFaucetTransaction>;
    fn set_priority(
        &self,
        node: FaucetSourceNode,
        txid: &str,
        desired_delta_sats: i64,
    ) -> anyhow::Result<PriorityUpdate>;
    fn test_accept(&self, node: FaucetSourceNode, raw_tx_hex: &str) -> anyhow::Result<()>;
    fn submit(&self, node: FaucetSourceNode, raw_tx_hex: &str, txid: &str) -> anyhow::Result<bool>;
    fn verify_miner(&self, node: FaucetSourceNode, txid: &str)
        -> anyhow::Result<MinerVerification>;
    fn observer_contains_unconfirmed(&self, txid: &str) -> anyhow::Result<bool>;
    fn confirmation(&self, txid: &str) -> anyhow::Result<Option<FaucetConfirmation>>;
    fn inputs_unspent(
        &self,
        source: FaucetSourceNode,
        inputs: &[FaucetInput],
    ) -> anyhow::Result<bool>;
}

pub struct RpcFaucetBackend {
    node1: Client,
    node2: Client,
    node3: Client,
    wallet2: Client,
    wallet3: Client,
}

impl RpcFaucetBackend {
    pub fn from_config(config: &ControlPlaneConfig) -> anyhow::Result<Self> {
        Ok(Self {
            node1: simchain_common::create_client(&config.node1_url)?,
            node2: simchain_common::create_client(&config.node2_url)?,
            node3: simchain_common::create_client(&config.node3_url)?,
            wallet2: simchain_common::create_wallet_client(
                &config.node2_url,
                &config.node2_wallet_name,
            )?,
            wallet3: simchain_common::create_wallet_client(
                &config.node3_url,
                &config.node3_wallet_name,
            )?,
        })
    }

    fn node(&self, node: FaucetSourceNode) -> &Client {
        match node {
            FaucetSourceNode::Node2 => &self.node2,
            FaucetSourceNode::Node3 => &self.node3,
        }
    }

    fn wallet(&self, node: FaucetSourceNode) -> &Client {
        match node {
            FaucetSourceNode::Node2 => &self.wallet2,
            FaucetSourceNode::Node3 => &self.wallet3,
        }
    }

    fn eligible_inputs(wallet: &Client) -> anyhow::Result<Vec<FaucetInput>> {
        // Core 31 no longer returns the legacy `balance` member expected by the
        // bitcoincore-rpc 0.19 list-unspent DTO. Decode only the stable fields used
        // for faucet selection while retaining exact BTC-to-satoshi conversion.
        let entries: Vec<ListUnspentEntry> = wallet.call(
            "listunspent",
            &[json!(1), json!(9_999_999), json!([]), json!(false)],
        )?;
        let mut inputs = entries
            .into_iter()
            .filter(|entry| entry.spendable && entry.safe && entry.confirmations > 0)
            .map(|entry| FaucetInput {
                txid: entry.txid.to_string(),
                vout: entry.vout,
                amount_sats: entry.amount.to_sat(),
                confirmations: entry.confirmations,
            })
            .collect::<Vec<_>>();
        sort_inputs(&mut inputs);
        Ok(inputs)
    }

    fn current_priority(client: &Client, txid: &str) -> anyhow::Result<i64> {
        let priorities: Value = client.call("getprioritisedtransactions", &[])?;
        let Some(entry) = priorities.get(txid) else {
            return Ok(0);
        };
        entry
            .get("fee_delta")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("priority entry for {txid} has no integer fee_delta"))
    }
}

impl FaucetBackend for RpcFaucetBackend {
    fn preflight(&self) -> anyhow::Result<FaucetPreflight> {
        let node1 = self.node1.get_blockchain_info()?;
        let node2 = self.node2.get_blockchain_info()?;
        let node3 = self.node3.get_blockchain_info()?;
        anyhow::ensure!(
            node1.chain == bitcoincore_rpc::bitcoin::Network::Regtest,
            "node1 is not regtest"
        );
        anyhow::ensure!(
            node1.blocks >= 204,
            "bootstrap is incomplete at height {}",
            node1.blocks
        );
        anyhow::ensure!(
            node1.best_block_hash == node2.best_block_hash
                && node1.best_block_hash == node3.best_block_hash,
            "node tips disagree (node1={}, node2={}, node3={})",
            node1.best_block_hash,
            node2.best_block_hash,
            node3.best_block_hash
        );
        Ok(FaucetPreflight {
            height: node1.blocks,
            best_hash: node1.best_block_hash.to_string(),
            node2_inputs: Self::eligible_inputs(&self.wallet2)?,
            node3_inputs: Self::eligible_inputs(&self.wallet3)?,
        })
    }

    fn lock_inputs(&self, source: FaucetSourceNode, inputs: &[FaucetInput]) -> anyhow::Result<()> {
        let outpoints = input_outpoints(inputs)?;
        anyhow::ensure!(
            self.wallet(source).lock_unspent(&outpoints)?,
            "wallet refused input lock"
        );
        Ok(())
    }

    fn unlock_inputs(
        &self,
        source: FaucetSourceNode,
        inputs: &[FaucetInput],
    ) -> anyhow::Result<()> {
        let outpoints = input_outpoints(inputs)?;
        anyhow::ensure!(
            self.wallet(source).unlock_unspent(&outpoints)?,
            "wallet refused input unlock"
        );
        Ok(())
    }

    fn prepare_transaction(
        &self,
        source: FaucetSourceNode,
        inputs: &[FaucetInput],
        outputs: &[FaucetOutput],
    ) -> anyhow::Result<PreparedFaucetTransaction> {
        let wallet = self.wallet(source);
        let input_sats = checked_input_total(inputs)?;
        let recipient_sats = checked_output_total(outputs)?;
        anyhow::ensure!(
            input_sats > recipient_sats,
            "selected inputs do not provide non-dust change"
        );
        let change_sats = input_sats - recipient_sats;
        let change =
            simchain_common::require_regtest_address(wallet.get_raw_change_address(None)?)?;
        let change_script = change.script_pubkey();
        anyhow::ensure!(
            change_sats >= change_script.minimal_non_dust().to_sat(),
            "selected input change is dust"
        );

        let mut tx_outputs = Vec::with_capacity(outputs.len() + 1);
        for output in outputs {
            let address =
                Address::<NetworkUnchecked>::from_str(&output.address).map_err(|error| {
                    anyhow::anyhow!("invalid destination {}: {error}", output.address)
                })?;
            let address = simchain_common::require_regtest_address(address).map_err(|error| {
                anyhow::anyhow!("destination {} is not regtest: {error}", output.address)
            })?;
            tx_outputs.push(TxOut {
                value: Amount::from_sat(output.amount_sats),
                script_pubkey: address.script_pubkey(),
            });
        }
        tx_outputs.push(TxOut {
            value: Amount::from_sat(change_sats),
            script_pubkey: change_script.clone(),
        });
        let unsigned = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: input_outpoints(inputs)?
                .into_iter()
                .map(|previous_output| TxIn {
                    previous_output,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                })
                .collect(),
            output: tx_outputs,
        };
        let signed = wallet.sign_raw_transaction_with_wallet(&unsigned, None, None)?;
        anyhow::ensure!(
            signed.complete,
            "wallet did not completely sign faucet transaction"
        );
        anyhow::ensure!(
            signed.errors.as_ref().is_none_or(Vec::is_empty),
            "wallet reported signing errors"
        );
        let transaction = signed.transaction()?;
        verify_prepared_transaction(
            &transaction,
            inputs,
            outputs,
            &change_script,
            input_sats,
            change_sats,
        )?;
        let vsize = transaction.vsize() as u64;
        anyhow::ensure!(
            vsize <= FAUCET_MAX_TX_VBYTES,
            "faucet transaction is {vsize} vB; maximum is {FAUCET_MAX_TX_VBYTES} vB"
        );
        Ok(PreparedFaucetTransaction {
            raw_tx_hex: serialize_hex(&transaction),
            txid: transaction.compute_txid().to_string(),
            input_sats,
            change_sats,
            vsize,
        })
    }

    fn set_priority(
        &self,
        node: FaucetSourceNode,
        txid: &str,
        desired_delta_sats: i64,
    ) -> anyhow::Result<PriorityUpdate> {
        let client = self.node(node);
        let previous = Self::current_priority(client, txid)?;
        let difference = desired_delta_sats
            .checked_sub(previous)
            .ok_or_else(|| anyhow::anyhow!("priority delta difference overflow"))?;
        if difference != 0 {
            let accepted: bool = client.call(
                "prioritisetransaction",
                &[json!(txid), Value::Null, json!(difference)],
            )?;
            anyhow::ensure!(accepted, "miner rejected priority update");
        }
        let actual = Self::current_priority(client, txid)?;
        anyhow::ensure!(
            actual == desired_delta_sats,
            "priority verification failed: wanted {desired_delta_sats}, got {actual}"
        );
        Ok(PriorityUpdate {
            previous_delta_sats: previous,
            desired_delta_sats,
        })
    }

    fn test_accept(&self, node: FaucetSourceNode, raw_tx_hex: &str) -> anyhow::Result<()> {
        let transaction: Transaction = deserialize_hex(raw_tx_hex)?;
        let result = self.node(node).test_mempool_accept(&[&transaction])?;
        let result = result
            .first()
            .ok_or_else(|| anyhow::anyhow!("testmempoolaccept returned no result"))?;
        anyhow::ensure!(
            result.allowed,
            "miner rejected faucet transaction: {}",
            result.reject_reason.as_deref().unwrap_or("unknown reason")
        );
        anyhow::ensure!(
            result
                .fees
                .as_ref()
                .is_some_and(|fees| fees.base.to_sat() == 0),
            "admission did not report an exact zero base fee"
        );
        Ok(())
    }

    fn submit(&self, node: FaucetSourceNode, raw_tx_hex: &str, txid: &str) -> anyhow::Result<bool> {
        let expected = Txid::from_str(txid)?;
        let current = self.node(node).get_raw_mempool()?;
        if current.contains(&expected) {
            return Ok(true);
        }
        let submitted: Txid = self
            .node(node)
            .call("sendrawtransaction", &[json!(raw_tx_hex), json!(0)])?;
        anyhow::ensure!(
            submitted == expected,
            "miner returned unexpected txid {submitted}"
        );
        Ok(false)
    }

    fn verify_miner(
        &self,
        node: FaucetSourceNode,
        txid: &str,
    ) -> anyhow::Result<MinerVerification> {
        let txid = Txid::from_str(txid)?;
        let client = self.node(node);
        let entry = client.get_mempool_entry(&txid)?;
        let delta = Self::current_priority(client, &txid.to_string())?;
        let info = client.get_mempool_info()?;
        let competitors = client.get_raw_mempool_verbose()?;
        let greatest = competitors
            .iter()
            .filter(|(other, _)| **other != txid)
            .map(|(_, entry)| div_ceil(entry.fees.modified.to_sat(), entry.vsize.max(1)))
            .max()
            .unwrap_or(0);
        Ok(MinerVerification {
            base_fee_sats: entry.fees.base.to_sat(),
            modified_fee_sats: entry.fees.modified.to_sat(),
            fee_delta_sats: delta,
            vsize: entry.vsize,
            weight: entry.weight,
            ancestor_count: entry.ancestor_count,
            greatest_competing_feerate_sat_vb: greatest,
            minimum_feerate_sat_vb: div_ceil(
                info.mempool_min_fee.max(info.min_relay_tx_fee).to_sat(),
                1_000,
            ),
        })
    }

    fn observer_contains_unconfirmed(&self, txid: &str) -> anyhow::Result<bool> {
        let txid = Txid::from_str(txid)?;
        Ok(self.node1.get_raw_mempool()?.contains(&txid))
    }

    fn confirmation(&self, txid: &str) -> anyhow::Result<Option<FaucetConfirmation>> {
        let txid = Txid::from_str(txid)?;
        let mempool = self.node1.get_raw_mempool()?;
        if mempool.contains(&txid) {
            return Ok(None);
        }
        let info = match self.node1.get_raw_transaction_info(&txid, None) {
            Ok(info) => info,
            Err(bitcoincore_rpc::Error::JsonRpc(bitcoincore_rpc::jsonrpc::error::Error::Rpc(
                error,
            ))) if error.code == -5 => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(block_hash) = info.blockhash else {
            return Ok(None);
        };
        if info.confirmations.unwrap_or(0) == 0 {
            return Ok(None);
        }
        let header = self.node1.get_block_header_info(&block_hash)?;
        Ok(Some(FaucetConfirmation {
            height: header.height as u64,
            block_hash: block_hash.to_string(),
        }))
    }

    fn inputs_unspent(
        &self,
        source: FaucetSourceNode,
        inputs: &[FaucetInput],
    ) -> anyhow::Result<bool> {
        for input in inputs {
            let outpoint = input.outpoint()?;
            let Some(txout) =
                self.node(source)
                    .get_tx_out(&outpoint.txid, outpoint.vout, Some(false))?
            else {
                return Ok(false);
            };
            if txout.value.to_sat() != input.amount_sats {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub fn select_inputs(
    candidates: &[FaucetInput],
    recipient_sats: u64,
    reserve_sats: u64,
) -> anyhow::Result<Vec<FaucetInput>> {
    let total = checked_input_total(candidates)?;
    let minimum_change = 546_u64;
    let required = recipient_sats
        .checked_add(minimum_change)
        .and_then(|value| value.checked_add(reserve_sats))
        .ok_or_else(|| anyhow::anyhow!("faucet amount overflow"))?;
    anyhow::ensure!(total >= required, "eligible funds {total} sats cannot cover {recipient_sats} sats plus {reserve_sats} sats reserve and non-dust change");

    let mut sorted = candidates.to_vec();
    sort_inputs(&mut sorted);
    let mut selected = Vec::new();
    let mut selected_total = 0_u64;
    for input in sorted {
        selected_total = selected_total
            .checked_add(input.amount_sats)
            .ok_or_else(|| anyhow::anyhow!("selected input amount overflow"))?;
        selected.push(input);
        if selected_total >= recipient_sats + minimum_change
            && total - selected_total >= reserve_sats
        {
            return Ok(selected);
        }
    }
    anyhow::bail!("no deterministic input selection preserves the wallet reserve")
}

pub fn eligible_total(inputs: &[FaucetInput]) -> anyhow::Result<u64> {
    checked_input_total(inputs)
}

fn sort_inputs(inputs: &mut [FaucetInput]) {
    inputs.sort_by(|left, right| {
        right
            .amount_sats
            .cmp(&left.amount_sats)
            .then_with(|| right.confirmations.cmp(&left.confirmations))
            .then_with(|| left.txid.cmp(&right.txid))
            .then_with(|| left.vout.cmp(&right.vout))
    });
}

fn input_outpoints(inputs: &[FaucetInput]) -> anyhow::Result<Vec<OutPoint>> {
    inputs.iter().map(FaucetInput::outpoint).collect()
}

fn checked_input_total(inputs: &[FaucetInput]) -> anyhow::Result<u64> {
    inputs.iter().try_fold(0_u64, |total, input| {
        total
            .checked_add(input.amount_sats)
            .ok_or_else(|| anyhow::anyhow!("input amount overflow"))
    })
}

fn checked_output_total(outputs: &[FaucetOutput]) -> anyhow::Result<u64> {
    outputs.iter().try_fold(0_u64, |total, output| {
        total
            .checked_add(output.amount_sats)
            .ok_or_else(|| anyhow::anyhow!("output amount overflow"))
    })
}

fn verify_prepared_transaction(
    transaction: &Transaction,
    inputs: &[FaucetInput],
    outputs: &[FaucetOutput],
    change_script: &ScriptBuf,
    input_sats: u64,
    change_sats: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        transaction.version == Version::TWO,
        "faucet transaction version changed during signing"
    );
    anyhow::ensure!(
        transaction.lock_time == LockTime::ZERO,
        "faucet transaction locktime changed during signing"
    );
    anyhow::ensure!(
        transaction
            .input
            .iter()
            .all(|input| input.sequence == Sequence::MAX),
        "faucet transaction must be final and non-RBF"
    );
    let actual_inputs = transaction
        .input
        .iter()
        .map(|input| input.previous_output)
        .collect::<HashSet<_>>();
    let expected_inputs = input_outpoints(inputs)?.into_iter().collect::<HashSet<_>>();
    anyhow::ensure!(
        actual_inputs == expected_inputs,
        "signed transaction inputs differ from selected inputs"
    );

    let mut expected_outputs = Vec::with_capacity(outputs.len() + 1);
    for output in outputs {
        let address = simchain_common::require_regtest_address(
            Address::<NetworkUnchecked>::from_str(&output.address)?,
        )?;
        expected_outputs.push((address.script_pubkey(), output.amount_sats));
    }
    expected_outputs.push((change_script.clone(), change_sats));
    let actual_outputs = transaction
        .output
        .iter()
        .map(|output| (output.script_pubkey.clone(), output.value.to_sat()))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        actual_outputs == expected_outputs,
        "signed transaction outputs differ from canonical request and change"
    );
    let output_sats = transaction.output.iter().try_fold(0_u64, |total, output| {
        total
            .checked_add(output.value.to_sat())
            .ok_or_else(|| anyhow::anyhow!("signed output amount overflow"))
    })?;
    anyhow::ensure!(
        output_sats == input_sats,
        "faucet transaction actual fee is not zero"
    );
    Ok(())
}

fn div_ceil(value: u64, divisor: u64) -> u64 {
    value.div_ceil(divisor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(txid_byte: &str, amount_sats: u64, confirmations: u32) -> FaucetInput {
        FaucetInput {
            txid: txid_byte.repeat(64),
            vout: 0,
            amount_sats,
            confirmations,
        }
    }

    #[test]
    fn deterministic_selection_prefers_value_then_confirmations_then_outpoint() {
        let inputs = vec![
            input("b", 700, 2),
            input("a", 700, 3),
            input("c", 500, 100),
            input("d", 1_000, 1),
        ];
        let selected = select_inputs(&inputs, 1_000, 500).unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].amount_sats, 1_000);
        assert_eq!(selected[1].txid, "a".repeat(64));
    }

    #[test]
    fn selection_preserves_reserve_and_non_dust_change() {
        let inputs = vec![input("a", 1_000, 1), input("b", 1_000, 1)];
        assert!(select_inputs(&inputs, 400, 500).is_ok());
        assert!(select_inputs(&inputs, 1_000, 500).is_err());
    }

    #[test]
    fn constants_pin_exact_zero_priority_band() {
        assert_eq!(
            simchain_common::control_api::FAUCET_PRIORITY_DELTA_SATS,
            10_000_000_000
        );
        assert_eq!(
            simchain_common::control_api::FAUCET_PRIORITY_DELTA_SATS as u64 / FAUCET_MAX_TX_VBYTES,
            1_000_000
        );
    }
}
