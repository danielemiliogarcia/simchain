//! Raw-transaction spam engine: the spammer owns its keys, tracks its own
//! UTXO set in memory, builds and signs every transaction locally (P2WPKH)
//! and submits it with sendrawtransaction -- the node wallets are bypassed
//! entirely. This removes the two wallet-engine ceilings at once: the wallet
//! lock (coin selection + signing serialized inside bitcoind) and wallet
//! fatigue (coin selection cost growing with tx history), so the cycle time
//! stays flat no matter how long the simulation runs.
//!
//! Bookkeeping is trivial because the engine never needs to *discover* coins:
//! it initiates every transaction that pays it (funding pulls from a miner
//! wallet, its own fan-outs, its own change), so the in-memory set is updated
//! locally on each send and stays at a constant size. Chain scans
//! (scantxoutset) are only a recovery path: startup and reorgs.
//!
//! Two spam shapes fill blocks:
//!   - OUTPUT mode (SPAM_TX_DATA_MAX_BYTES = 0): each tx has burn outputs
//!     (546-sat P2WPKH), one in sequential mode or SPAM_SENDMANY_OUTPUTS in
//!     batch mode -- exchange-payout-shaped, but every output is a UTXO-set
//!     insert for the nodes. Driven by a fixed tx count.
//!   - DATA/HYBRID mode (SPAM_TX_DATA_MAX_BYTES > 0): the fill comes from
//!     OP_RETURN data txs whose payload size is drawn log-uniformly between
//!     SPAM_TX_DATA_MIN_BYTES and _MAX_BYTES, giving a realistic spread of tx
//!     sizes at near-zero node cost (OP_RETURN never enters the UTXO set).
//!     Each block also gets a guaranteed batch of minimum-size P2WPKH
//!     gap-sealer txs (SPAM_SMALL_TXS_PER_BLOCK) so leftover block space is
//!     always taken by a floor-priced tx -- a cheap user tx then has to
//!     *outbid* the floor, not slip through an unused gap. The engine fills to
//!     a target of SPAM_FILL_BLOCK_RATIO blocks of mempool weight, so the same
//!     mode does partial blocks (ratio < 1), just-full blocks (ratio 1) and a
//!     deep visible mempool backlog (ratio > 1). Needs Bitcoin Core 30+ (large
//!     OP_RETURN standard by default).

use crate::common;
use bitcoincore_rpc::{
    bitcoin::{
        absolute::LockTime,
        consensus::encode::serialize_hex,
        ecdsa,
        hashes::{sha256, Hash},
        script::PushBytesBuf,
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

// 546-sat burn/gap-sealer outputs: safely above the P2PKH dust floor for any
// address type, same amount as the wallet engine so shapes match.
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

// The three spam-tx shapes the engine builds. The shape is enough to recompute
// the tx's vsize, fee, non-change value and outputs, so a SentSpam only needs
// to carry the shape to rebuild the tx for an RBF bump.
#[derive(Clone)]
enum SpamShape {
    // OP_RETURN of N data bytes (value 0). DATA/HYBRID bulk fill.
    Data(usize),
    // One minimum-size P2WPKH burn output. HYBRID gap-sealer / small tx.
    Sealer,
    // The OUTPUT-mode burn outputs (1 in sequential, N in batch mode).
    Burns,
}

// Everything needed to RBF-bump a spam tx after the fact: the input it spent
// (sighash needs the spent amount), what it paid, and its shape (to rebuild).
struct SentSpam {
    txid: Txid,
    spent: Utxo,
    fee: Amount,
    change: Amount,
    shape: SpamShape,
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
    // OUTPUT-mode burn scripts (one, or SPAM_SENDMANY_OUTPUTS of them).
    burn_scripts: Vec<ScriptBuf>,
    // A single P2WPKH burn script for the minimum-size gap-sealer txs.
    sealer_script: ScriptBuf,
    // DATA/HYBRID mode range. data_max == 0 means OUTPUT mode. data_min == 0
    // (or >= data_max) means every data tx is exactly data_max (uniform);
    // 0 < data_min < data_max draws each size log-uniformly in [min, max].
    data_min: usize,
    data_max: usize,
    // Monotonic counter feeding the deterministic log-uniform size draw, so
    // sizes vary within and across blocks without an RNG dependency.
    draw_counter: u64,
    utxos: Vec<Utxo>,
    cursor: usize,
}

// vsize of an OP_RETURN data tx: 1 P2WPKH input (~68 vB incl. witness), 1
// change output (31 vB), ~11 vB overhead, plus the OP_RETURN output's full
// base size (value + scriptlen varint + OP_RETURN + pushdata prefix + data).
fn data_tx_vsize(n: usize) -> u64 {
    let push_prefix = match n {
        0..=75 => 1,
        76..=255 => 2,
        256..=65535 => 3,
        _ => 5,
    };
    let script_len = 1 + push_prefix + n;
    let varint = if script_len < 253 {
        1
    } else if script_len < 65536 {
        3
    } else {
        5
    };
    let op_return_out = 8 + varint + script_len;
    (11 + 68 + 31 + op_return_out) as u64
}

fn op_return_script(n: usize) -> ScriptBuf {
    let mut payload = PushBytesBuf::new();
    payload
        .extend_from_slice(&vec![0xab_u8; n])
        .expect("data payload within push-size limit");
    ScriptBuf::new_op_return(payload)
}

impl RawSpammer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        node: Client,
        wallet: Client,
        wallet_name: &str,
        label: &str,
        fee_rate_sat_vb: f64,
        sendmany_outputs: u64,
        data_min: u64,
        data_max: u64,
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
        let sealer_script = common::burn_address(0).script_pubkey();
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
            sealer_script,
            data_min: data_min as usize,
            data_max: data_max as usize,
            draw_counter: 0,
            utxos: Vec::new(),
            cursor: 0,
        }
    }

    fn fee_from_vsize(&self, vsize: u64) -> Amount {
        Amount::from_sat((vsize as f64 * self.fee_rate_sat_vb).ceil() as u64)
    }

    fn shape_vsize(&self, shape: &SpamShape) -> u64 {
        match shape {
            SpamShape::Data(n) => data_tx_vsize(*n),
            // 1 input + 1 burn output + 1 change output + overhead
            SpamShape::Sealer => 11 + 68 + 31 + 31,
            SpamShape::Burns => {
                let k = self.burn_scripts.len();
                (11 + 68 + 31 * (k + 1) + if k + 1 >= 253 { 2 } else { 0 }) as u64
            }
        }
    }

    fn shape_fee(&self, shape: &SpamShape) -> Amount {
        self.fee_from_vsize(self.shape_vsize(shape))
    }

    // Total value carried by the non-change outputs (all dust; the real cost
    // is the fee). Data txs carry none -- the OP_RETURN has value 0.
    fn shape_nonchange_value(&self, shape: &SpamShape) -> Amount {
        match shape {
            SpamShape::Data(_) => Amount::ZERO,
            SpamShape::Sealer => Amount::from_sat(DUST_SAT),
            SpamShape::Burns => Amount::from_sat(DUST_SAT * self.burn_scripts.len() as u64),
        }
    }

    // Change is always the LAST output, so its vout is the number of
    // non-change outputs before it.
    fn shape_change_vout(&self, shape: &SpamShape) -> u32 {
        match shape {
            SpamShape::Data(_) => 1,
            SpamShape::Sealer => 1,
            SpamShape::Burns => self.burn_scripts.len() as u32,
        }
    }

    fn build_outputs(&self, shape: &SpamShape, change: Amount) -> Vec<TxOut> {
        let mut outputs: Vec<TxOut> = match shape {
            SpamShape::Data(n) => vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: op_return_script(*n),
            }],
            SpamShape::Sealer => vec![TxOut {
                value: Amount::from_sat(DUST_SAT),
                script_pubkey: self.sealer_script.clone(),
            }],
            SpamShape::Burns => self
                .burn_scripts
                .iter()
                .map(|script| TxOut {
                    value: Amount::from_sat(DUST_SAT),
                    script_pubkey: script.clone(),
                })
                .collect(),
        };
        outputs.push(TxOut {
            value: change,
            script_pubkey: self.script_pubkey.clone(),
        });
        outputs
    }

    // Log-uniform payload size in [data_min, data_max]: equal weight per order
    // of magnitude, so most txs are small and a few are large, like a real
    // mempool. Deterministic (a multiplicative hash of a running counter), so
    // no RNG dependency; the sizes still vary within and across blocks.
    fn draw_data_size(&mut self) -> usize {
        if self.data_min == 0 || self.data_min >= self.data_max {
            return self.data_max;
        }
        let c = self.draw_counter;
        self.draw_counter = self.draw_counter.wrapping_add(1);
        let h = (c as u32).wrapping_mul(2_654_435_761);
        let frac = h as f64 / u32::MAX as f64;
        let lo = self.data_min as f64;
        let hi = self.data_max as f64;
        let size = lo * (hi / lo).powf(frac);
        (size.round() as usize).clamp(self.data_min, self.data_max)
    }

    // The biggest single tx a branch must be able to afford: a max-size data
    // tx in DATA mode, a full burn tx in OUTPUT mode. Used to size the branch
    // pool and to pick branches able to send.
    fn per_tx_required(&self) -> Amount {
        if self.data_max > 0 {
            self.shape_fee(&SpamShape::Data(self.data_max)) + MIN_CHANGE
        } else {
            self.shape_fee(&SpamShape::Burns)
                + self.shape_nonchange_value(&SpamShape::Burns)
                + MIN_CHANGE
        }
    }

    fn usable_branches(&self, required: Amount) -> u64 {
        self.utxos.iter().filter(|u| u.amount >= required).count() as u64
    }

    // Fee for the fan-out consolidation tx: many inputs, `n_out` change-style
    // outputs, no data.
    fn consolidation_fee(&self, n_in: usize, n_out: usize) -> Amount {
        self.fee_from_vsize((11 + 68 * n_in + 31 * n_out) as u64)
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
        let fee = self.consolidation_fee(self.utxos.len(), target as usize);
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

    // Build, sign and send one tx of the given shape from the next usable
    // branch, updating that branch's tip to the tx's change output. Returns
    // the SentSpam record (for RBF) or a classified error string.
    fn send_shape(&mut self, shape: SpamShape, replaceable: bool) -> Result<SentSpam, String> {
        let required = self.per_tx_required();
        let Some(idx) = self.next_branch(required) else {
            return Err("no usable branch".to_string());
        };
        let branch = self.utxos[idx];
        let fee = self.shape_fee(&shape);
        let nonchange = self.shape_nonchange_value(&shape);
        let change = match branch.amount.checked_sub(nonchange + fee) {
            Some(c) if c >= MIN_CHANGE => c,
            _ => return Err("branch too small for this tx".to_string()),
        };
        let vout = self.shape_change_vout(&shape);
        let outputs = self.build_outputs(&shape, change);
        match self.send_tx(std::slice::from_ref(&branch), outputs, replaceable) {
            Ok(txid) => {
                self.utxos[idx] = Utxo {
                    outpoint: OutPoint::new(txid, vout),
                    amount: change,
                };
                Ok(SentSpam {
                    txid,
                    spent: branch,
                    fee,
                    change,
                    shape,
                })
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("missing") || msg.contains("conflict") || msg.contains("spent") {
                    // Our view of this branch is stale (a reorg or restart
                    // raced us): forget it, resync picks up the truth next
                    // shortage.
                    self.utxos.remove(idx);
                    if !self.utxos.is_empty() {
                        self.cursor %= self.utxos.len();
                    }
                }
                Err(msg)
            }
        }
    }

    // Fee-bump (RBF) up to `count` of the just-sent spam txs: rebuild the same
    // spend (same shape) with double the fee (change shrinks by the old fee,
    // clearing BIP125's +1 sat/vB minimum), re-sign, broadcast. Only branch
    // TIPS can be replaced -- if a later tx already chained off this one's
    // change, replacing it would orphan that child -- and the tip check is
    // simply "is this tx's change outpoint still in our UTXO set".
    fn bump_spam_txs(&mut self, sent: &[SentSpam], count: u64) {
        let mut bumped = 0;
        let mut first_error: Option<String> = None;
        for s in sent.iter().rev() {
            if bumped >= count {
                break;
            }
            let vout = self.shape_change_vout(&s.shape);
            let tip = OutPoint::new(s.txid, vout);
            let Some(idx) = self.utxos.iter().position(|u| u.outpoint == tip) else {
                continue;
            };
            let Some(new_change) = s.change.checked_sub(s.fee) else {
                continue;
            };
            if new_change < MIN_CHANGE {
                continue;
            }
            let outputs = self.build_outputs(&s.shape, new_change);
            match self.send_tx(std::slice::from_ref(&s.spent), outputs, true) {
                Ok(txid) => {
                    self.utxos[idx] = Utxo {
                        outpoint: OutPoint::new(txid, vout),
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

    // OUTPUT mode: send this node's fixed share of burn-output spam txs
    // (sequential or batch, depending on the burn-script count), then fee-bump
    // its own txs when RBF is enabled.
    pub fn output_round(
        &mut self,
        share: u64,
        fanout: u64,
        replaceable: bool,
        replaces: u64,
    ) -> Vec<Txid> {
        if fanout > 0 {
            self.ensure_funds(share.min(fanout), fanout);
        }
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

        let mut txids = Vec::new();
        let mut sent = Vec::new();
        let mut first_error: Option<String> = None;
        let mut consecutive_failures = 0;
        while (txids.len() as u64) < share {
            if self.utxos.is_empty() || consecutive_failures >= self.utxos.len() {
                break;
            }
            match self.send_shape(SpamShape::Burns, replaceable) {
                Ok(s) => {
                    txids.push(s.txid);
                    sent.push(s);
                    consecutive_failures = 0;
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
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

    // DATA/HYBRID mode: send `small_txs` guaranteed minimum-size gap-sealer
    // txs (so the fee floor holds), then fill with varied-size OP_RETURN data
    // txs until `deficit_vsize` of weight has been offered (or the branch pool
    // is exhausted). `deficit_vsize` is how much the caller wants added to the
    // mempool this block to reach the SPAM_FILL_BLOCK_RATIO target. Returns the
    // txids and the total vsize actually offered.
    pub fn hybrid_round(
        &mut self,
        deficit_vsize: u64,
        small_txs: u64,
        fanout: u64,
        replaceable: bool,
        replaces: u64,
    ) -> (Vec<Txid>, u64) {
        self.ensure_funds(fanout, fanout);

        let mut txids: Vec<Txid> = Vec::new();
        let mut sent: Vec<SentSpam> = Vec::new();
        let mut added: u64 = 0;
        let mut first_error: Option<String> = None;
        let mut sealer_count = 0u64;
        let mut data_count = 0u64;

        // Guaranteed gap-sealers first: minimum-size floor-priced txs that take
        // any leftover block space before a cheap user tx can.
        let mut fails = 0;
        while sealer_count < small_txs {
            if self.utxos.is_empty() || fails >= self.utxos.len() {
                break;
            }
            match self.send_shape(SpamShape::Sealer, replaceable) {
                Ok(s) => {
                    added += self.shape_vsize(&s.shape);
                    txids.push(s.txid);
                    sent.push(s);
                    sealer_count += 1;
                    fails = 0;
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                    fails += 1;
                }
            }
        }

        // Bulk fill with varied-size data txs up to the requested weight.
        fails = 0;
        while added < deficit_vsize {
            if self.utxos.is_empty() || fails >= self.utxos.len() {
                break;
            }
            let size = self.draw_data_size();
            match self.send_shape(SpamShape::Data(size), replaceable) {
                Ok(s) => {
                    added += self.shape_vsize(&s.shape);
                    txids.push(s.txid);
                    sent.push(s);
                    data_count += 1;
                    fails = 0;
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                    fails += 1;
                }
            }
        }

        println!(
            "{} => Hybrid: {sealer_count} gap-sealers + {data_count} data txs, ~{}k vB offered",
            self.label,
            added / 1000
        );
        if added < deficit_vsize {
            let detail = first_error.map(|e| format!(", first error: {e}")).unwrap_or_else(|| {
                ", branch pool exhausted (raise SPAM_FANOUT_UTXOS / SPAM_FILL_BLOCK_RATIO headroom)"
                    .to_string()
            });
            println!(
                "WARNING: {} only offered ~{}k/{}k vB this block{detail}",
                self.label,
                added / 1000,
                deficit_vsize / 1000
            );
        }
        if replaceable {
            self.bump_spam_txs(&sent, replaces);
        }
        (txids, added)
    }
}
