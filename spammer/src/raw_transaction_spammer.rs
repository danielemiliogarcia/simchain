//! Raw-transaction spam engine: the spammer owns its keys, tracks its own
//! UTXO set in memory, builds and signs every transaction locally (P2WPKH)
//! and submits it with sendrawtransaction -- the node wallets are bypassed
//! entirely. This removes the two wallet-engine ceilings at once: the wallet
//! lock (coin selection + signing serialized inside bitcoind) and wallet
//! fatigue (coin selection cost growing with tx history), so the cycle time
//! stays flat no matter how long the simulation runs. The throughput ceiling
//! becomes mempool acceptance itself, which is how real spam waves operate.
//!
//! Bookkeeping is trivial because the engine never needs to *discover* coins:
//! it initiates every transaction that pays it (funding pulls from a miner
//! wallet, its own fan-outs, its own change), so the in-memory set is updated
//! locally on each send and stays at a constant size. Chain scans
//! (scantxoutset) are only a recovery path: startup and reorgs.

use crate::common;
use bitcoincore_rpc::{
    bitcoin::{
        absolute::LockTime,
        consensus::encode::serialize_hex,
        ecdsa,
        hashes::{sha256, Hash},
        secp256k1::{All, Message, PublicKey, Secp256k1, SecretKey},
        sighash::{EcdsaSighashType, SighashCache},
        transaction::Version,
        Address, Amount, CompressedPublicKey, Network, OutPoint, ScriptBuf, Sequence, Transaction,
        TxIn, TxOut, Txid, Witness,
    },
    json::ScanTxOutRequest,
    Client, RpcApi,
};
use serde_json::json;
use std::{thread, time::Duration};

// Same 546-sat burn outputs as the wallet engine, so both engines produce
// identically shaped spam and drain at the same rate.
const DUST_SAT: u64 = 546;

// Never let a change output drop below this; below ~294 sats a P2WPKH output
// is dust and the node rejects the whole transaction.
const MIN_CHANGE: Amount = Amount::from_sat(546);

// One funding pull from the miner wallet. Half the wallet's trusted balance,
// capped here: after bootstrap each miner wallet holds ~2550 BTC, and 500 BTC
// funds hundreds of full blocks even at a 100 sat/vB price level.
const FUND_PULL_MAX_BTC: f64 = 500.0;

// A branch must afford at least this many spam txs to count as usable when
// deciding whether the pool needs a refill/re-split.
const BRANCH_MIN_TXS: u64 = 16;

#[derive(Clone, Copy)]
struct Utxo {
    outpoint: OutPoint,
    amount: Amount,
}

// Everything needed to RBF-bump a spam tx after the fact: the input it spent
// (sighash needs the spent amount) and what it paid.
struct SentSpam {
    txid: Txid,
    spent: Utxo,
    fee: Amount,
    change: Amount,
}

pub struct RawSpammer {
    node: Client,
    wallet: Client,
    wallet_name: String,
    label: String,
    secp: Secp256k1<All>,
    secret: SecretKey,
    pubkey: PublicKey,
    address: Address,
    script_pubkey: ScriptBuf,
    fee_rate_sat_vb: f64,
    // The outputs of every spam tx: one burn script in sequential mode,
    // SPAM_SENDMANY_OUTPUTS of them in batch mode (same burn addresses as the
    // wallet engine). Change to self is appended as the LAST output, so the
    // change vout is always burn_scripts.len().
    burn_scripts: Vec<ScriptBuf>,
    utxos: Vec<Utxo>,
    cursor: usize,
}

impl RawSpammer {
    pub fn new(
        node: Client,
        wallet: Client,
        wallet_name: &str,
        label: &str,
        fee_rate_sat_vb: f64,
        sendmany_outputs: u64,
    ) -> Self {
        // Deterministic key (hash of a fixed tag): the same address across
        // restarts, so a restarted spammer recovers its previous coins with
        // scantxoutset instead of starting broke. Regtest-only money, so a
        // publicly derivable key is fine -- same spirit as the burn addresses.
        let secp = Secp256k1::new();
        let tag = sha256::Hash::hash(format!("simchain-raw-spam-{wallet_name}").as_bytes());
        let secret =
            SecretKey::from_slice(tag.as_byte_array()).expect("sha256 of tag is a valid key");
        let pubkey = PublicKey::from_secret_key(&secp, &secret);
        let address = Address::p2wpkh(&CompressedPublicKey(pubkey), Network::Regtest);
        let script_pubkey = address.script_pubkey();
        let burn_scripts: Vec<ScriptBuf> = if sendmany_outputs == 0 {
            vec![common::burn_address(0).script_pubkey()]
        } else {
            (1..=sendmany_outputs)
                .map(|i| common::burn_address(i).script_pubkey())
                .collect()
        };
        println!("{label} => Raw spam engine address: {address}");
        RawSpammer {
            node,
            wallet,
            wallet_name: wallet_name.to_string(),
            label: label.to_string(),
            secp,
            secret,
            pubkey,
            address,
            script_pubkey,
            fee_rate_sat_vb,
            burn_scripts,
            utxos: Vec::new(),
            cursor: 0,
        }
    }

    // Explicit fee, the raw engine's replacement for the wallet estimator:
    // vsize is known from the tx shape (P2WPKH input ~68 vB, P2WPKH output
    // 31 vB, ~11 vB fixed overhead, +2 when the output-count varint grows
    // past 252). The 68 assumes the largest signature encoding, so the
    // realized feerate is never below the configured one.
    fn fee_for(&self, n_inputs: usize, n_outputs: usize) -> Amount {
        let vsize = 11 + 68 * n_inputs + 31 * n_outputs + if n_outputs >= 253 { 2 } else { 0 };
        Amount::from_sat((vsize as f64 * self.fee_rate_sat_vb).ceil() as u64)
    }

    // What one spam tx costs a branch: burns + fee + a change output that
    // must stay above dust.
    fn per_tx_required(&self) -> Amount {
        let n = self.burn_scripts.len();
        Amount::from_sat(DUST_SAT * n as u64) + self.fee_for(1, n + 1) + MIN_CHANGE
    }

    fn usable_branches(&self, required: Amount) -> u64 {
        self.utxos.iter().filter(|u| u.amount >= required).count() as u64
    }

    // Build, sign and broadcast one transaction spending our own P2WPKH
    // UTXOs. maxfeerate=0 disables sendrawtransaction's 0.1 BTC/kvB safety
    // cap, so a deliberately high FALLBACK_FEE price level still broadcasts.
    fn send_tx(
        &self,
        inputs: &[Utxo],
        outputs: Vec<TxOut>,
        replaceable: bool,
    ) -> Result<Txid, bitcoincore_rpc::Error> {
        let sequence = if replaceable {
            Sequence::ENABLE_RBF_NO_LOCKTIME
        } else {
            Sequence::MAX
        };
        let mut tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: inputs
                .iter()
                .map(|u| TxIn {
                    previous_output: u.outpoint,
                    script_sig: ScriptBuf::new(),
                    sequence,
                    witness: Witness::default(),
                })
                .collect(),
            output: outputs,
        };
        let mut cache = SighashCache::new(&mut tx);
        for (i, utxo) in inputs.iter().enumerate() {
            let sighash = cache
                .p2wpkh_signature_hash(i, &self.script_pubkey, utxo.amount, EcdsaSighashType::All)
                .expect("valid p2wpkh sighash");
            let signature = ecdsa::Signature {
                signature: self
                    .secp
                    .sign_ecdsa(&Message::from_digest(sighash.to_byte_array()), &self.secret),
                sighash_type: EcdsaSighashType::All,
            };
            *cache.witness_mut(i).unwrap() = Witness::p2wpkh(&signature, &self.pubkey);
        }
        drop(cache);
        self.node
            .call::<String>("sendrawtransaction", &[json!(serialize_hex(&tx)), json!(0)])
            .map(|s| s.parse().expect("bitcoind returned an invalid txid"))
    }

    // Rebuild the UTXO set from the chain. scantxoutset only sees CONFIRMED
    // outputs, so two corrections apply: outputs already spent by our own
    // still-in-mempool txs are filtered out with gettxout(include_mempool),
    // and branches whose current tip is unconfirmed stay invisible until a
    // block confirms them (the next low-pool check picks them back up). Only
    // a recovery path -- startup, reorgs, lost track -- never the hot path.
    fn resync(&mut self) {
        let scan = self
            .node
            .scan_tx_out_set_blocking(&[ScanTxOutRequest::Single(format!(
                "addr({})",
                self.address
            ))])
            .unwrap();
        self.utxos = scan
            .unspents
            .into_iter()
            .filter(|u| {
                self.node
                    .get_tx_out(&u.txid, u.vout, Some(true))
                    .unwrap()
                    .is_some()
            })
            .map(|u| Utxo {
                outpoint: OutPoint::new(u.txid, u.vout),
                amount: u.amount,
            })
            .collect();
        self.cursor = 0;
    }

    // Keep the engine holding `target` independent branches able to spam.
    // Cheap in-memory check when healthy (safe every block); on shortage:
    // resync with the chain, pull a refill from the miner wallet if the total
    // is low, then consolidate everything (dust remnants included) into one
    // tx that re-splits into `target` equal branches. Waits for that fan-out
    // to confirm before returning: an unconfirmed parent caps its descendant
    // count at 25 (mempool policy), which would strangle the first blocks.
    fn ensure_funds(&mut self, need: u64, target: u64) {
        let required = self.per_tx_required();
        if self.usable_branches(required) >= need {
            return;
        }
        self.resync();
        if self.usable_branches(required) >= need {
            return;
        }

        let total: Amount = self.utxos.iter().map(|u| u.amount).sum();
        let refill_floor = required * (target * BRANCH_MIN_TXS);
        if total < refill_floor {
            common::wait_for_funds(&self.wallet, &self.wallet_name);
            let trusted = self.wallet.get_balances().unwrap().mine.trusted.to_btc();
            let pull_btc = ((trusted * 0.5).min(FUND_PULL_MAX_BTC) * 1e8).floor() / 1e8;
            let pull = Amount::from_btc(pull_btc).unwrap();
            println!(
                "{} => Raw engine pulling {pull} from wallet '{}'",
                self.label, self.wallet_name
            );
            let txid = self
                .wallet
                .send_to_address(&self.address, pull, None, None, None, None, None, None)
                .unwrap();
            while self
                .wallet
                .get_transaction(&txid, None)
                .map(|tx| tx.info.confirmations)
                .unwrap_or(0)
                < 1
            {
                thread::sleep(Duration::from_millis(500));
            }
            self.resync();
        }

        let total: Amount = self.utxos.iter().map(|u| u.amount).sum();
        if self.utxos.is_empty() {
            println!(
                "{} => Raw engine has no confirmed funds to fan out yet, deferring",
                self.label
            );
            return;
        }
        let fee = self.fee_for(self.utxos.len(), target as usize);
        let per_branch = match total.checked_sub(fee) {
            Some(split) => split / target,
            None => Amount::ZERO,
        };
        if per_branch < required {
            println!(
                "{} => Raw engine funds too low to split {total} into {target} usable branches, deferring",
                self.label
            );
            return;
        }

        println!(
            "{} => Raw engine splitting {total} into {target} branches of {per_branch}",
            self.label
        );
        let outputs: Vec<TxOut> = (0..target)
            .map(|_| TxOut {
                value: per_branch,
                script_pubkey: self.script_pubkey.clone(),
            })
            .collect();
        let inputs = std::mem::take(&mut self.utxos);
        match self.send_tx(&inputs, outputs, false) {
            Ok(txid) => {
                println!(
                    "{} => Fan-out tx {txid} sent, waiting for it to confirm...",
                    self.label
                );
                while !matches!(self.node.get_tx_out(&txid, 0, Some(false)), Ok(Some(_))) {
                    thread::sleep(Duration::from_millis(500));
                }
                self.utxos = (0..target)
                    .map(|i| Utxo {
                        outpoint: OutPoint::new(txid, i as u32),
                        amount: per_branch,
                    })
                    .collect();
                self.cursor = 0;
                println!("{} => Fan-out confirmed", self.label);
            }
            Err(e) => {
                println!(
                    "{} => Raw engine fan-out failed ({e}), retrying next block",
                    self.label
                );
                self.resync();
            }
        }
    }

    // Next branch (round-robin) that can afford one spam tx. Round-robin
    // spreads the block's spam evenly across branches, so no single
    // unconfirmed chain grows deep enough to hit the 25-tx/101kvB limits
    // before the others.
    fn next_branch(&mut self, required: Amount) -> Option<usize> {
        let n = self.utxos.len();
        for step in 0..n {
            let idx = (self.cursor + step) % n;
            if self.utxos[idx].amount >= required {
                self.cursor = (idx + 1) % n;
                return Some(idx);
            }
        }
        None
    }

    // Fee-bump (RBF) up to `count` of the just-sent spam txs, the raw
    // counterpart of the wallet engine's bumpfee calls: rebuild the same
    // spend with double the fee (change shrinks by the old fee, comfortably
    // clearing BIP125's +1 sat/vB minimum), re-sign, broadcast. Only branch
    // TIPS can be replaced -- if a later tx already chained off this one's
    // change, replacing it would orphan that child -- and the tip check is
    // simply "is this tx's change outpoint still in our UTXO set".
    fn bump_spam_txs(&mut self, sent: &[SentSpam], count: u64) {
        let change_vout = self.burn_scripts.len() as u32;
        let mut bumped = 0;
        let mut first_error: Option<String> = None;
        for s in sent.iter().rev() {
            if bumped >= count {
                break;
            }
            let tip = OutPoint::new(s.txid, change_vout);
            let Some(idx) = self.utxos.iter().position(|u| u.outpoint == tip) else {
                continue;
            };
            let Some(new_change) = s.change.checked_sub(s.fee) else {
                continue;
            };
            if new_change < MIN_CHANGE {
                continue;
            }
            let mut outputs: Vec<TxOut> = self
                .burn_scripts
                .iter()
                .map(|script| TxOut {
                    value: Amount::from_sat(DUST_SAT),
                    script_pubkey: script.clone(),
                })
                .collect();
            outputs.push(TxOut {
                value: new_change,
                script_pubkey: self.script_pubkey.clone(),
            });
            match self.send_tx(std::slice::from_ref(&s.spent), outputs, true) {
                Ok(txid) => {
                    self.utxos[idx] = Utxo {
                        outpoint: OutPoint::new(txid, change_vout),
                        amount: new_change,
                    };
                    bumped += 1;
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
        }
        match first_error {
            Some(error) if bumped < count => println!(
                "{} => Fee-bumped (RBF) {bumped}/{count} raw spam txs, first error: {error}",
                self.label
            ),
            _ => println!("{} => Fee-bumped (RBF) {bumped} raw spam txs", self.label),
        }
    }

    // One node's full spam round, the raw counterpart of the wallet engine's
    // spam_round: top up the branch pool if it ran low, send this node's
    // share of the block's spam (each tx: one branch input -> burn outputs +
    // change back to the branch), then fee-bump its own txs when RBF traffic
    // is enabled. Two instances run in parallel, one thread per node.
    pub fn spam_round(
        &mut self,
        share: u64,
        fanout_need: u64,
        fanout_target: u64,
        replaceable: bool,
        replaces: u64,
    ) -> Vec<Txid> {
        self.ensure_funds(fanout_need, fanout_target);

        let n_burns = self.burn_scripts.len();
        if n_burns == 1 {
            println!(
                "{} => Raw-spamming {share} transactions to a burn address",
                self.label
            );
        } else {
            println!(
                "{} => Raw-spamming {share} txs of {n_burns} outputs to burn addresses",
                self.label
            );
        }

        let required = self.per_tx_required();
        let fee = self.fee_for(1, n_burns + 1);
        let burn_total = Amount::from_sat(DUST_SAT * n_burns as u64);

        let mut txids: Vec<Txid> = Vec::new();
        let mut sent: Vec<SentSpam> = Vec::new();
        let mut first_error: Option<String> = None;
        // One failure per branch in a row means every branch is refusing
        // (chain limits, drained pool): give up on this block instead of
        // spinning; the confirmations in the next block clear the limits.
        let mut consecutive_failures = 0;

        while (txids.len() as u64) < share {
            if self.utxos.is_empty() || consecutive_failures >= self.utxos.len() {
                break;
            }
            let Some(idx) = self.next_branch(required) else {
                break;
            };
            let branch = self.utxos[idx];
            let change = branch.amount - burn_total - fee;
            let mut outputs: Vec<TxOut> = self
                .burn_scripts
                .iter()
                .map(|script| TxOut {
                    value: Amount::from_sat(DUST_SAT),
                    script_pubkey: script.clone(),
                })
                .collect();
            outputs.push(TxOut {
                value: change,
                script_pubkey: self.script_pubkey.clone(),
            });
            match self.send_tx(std::slice::from_ref(&branch), outputs, replaceable) {
                Ok(txid) => {
                    // The branch's new tip is this tx's change output.
                    self.utxos[idx] = Utxo {
                        outpoint: OutPoint::new(txid, n_burns as u32),
                        amount: change,
                    };
                    sent.push(SentSpam {
                        txid,
                        spent: branch,
                        fee,
                        change,
                    });
                    txids.push(txid);
                    consecutive_failures = 0;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("missing") || msg.contains("conflict") || msg.contains("spent")
                    {
                        // Our view of this branch is stale (a reorg or a
                        // restart raced us): forget it, resync picks up the
                        // truth next shortage.
                        self.utxos.remove(idx);
                        if !self.utxos.is_empty() {
                            self.cursor %= self.utxos.len();
                        }
                    }
                    // Other errors (too-long-mempool-chain, policy): the
                    // branch stays; it becomes spendable again after a block.
                    if first_error.is_none() {
                        first_error = Some(msg);
                    }
                    consecutive_failures += 1;
                }
            }
        }

        if (txids.len() as u64) < share {
            let detail = first_error
                .map(|e| format!(", first error: {e}"))
                .unwrap_or_else(|| ", branch pool exhausted".to_string());
            println!(
                "WARNING: only {}/{share} raw spam txs accepted{detail}",
                txids.len()
            );
        }
        if replaceable {
            self.bump_spam_txs(&sent, replaces);
        }
        txids
    }
}
