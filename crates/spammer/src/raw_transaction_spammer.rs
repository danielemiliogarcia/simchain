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
//!     gap-sealer txs (SPAM_SMALL_TXS_PER_BLOCK), small realistic-looking
//!     floor-priced traffic. The engine fills to a target of
//!     SPAM_FILL_BLOCK_RATIO blocks of mempool weight, so the same mode does
//!     partial blocks (ratio < 1), just-full blocks (ratio 1) and a deep
//!     visible mempool backlog (ratio > 1). Needs Bitcoin Core 30+ (large
//!     OP_RETURN standard by default).
//!
//! The airtight fee floor comes from a third piece, the FLOOR FILL POOL
//! (SPAM_FLOOR_POOL_TXS, DATA/HYBRID mode): a standing pool of standalone
//! floor-priced minimum-size self-transfers kept sitting in the mempool at all
//! times. Spam txs chain off unconfirmed change, so their ancestor packages
//! are far too big to fit residual block gaps; a fill instead spends a
//! CONFIRMED UTXO from a dedicated second key, so its ancestor package is
//! itself and the block assembler can drop it into those gaps. With enough
//! standing fills, blocks pack down to a fill-sized remainder and a below-floor
//! tx has no gap left to slip into -- it must outbid the floor.
//! Mined fills confirm their change, which becomes fresh pool ammo: the pool
//! churns 1:1 with zero net UTXO-set growth.

use crate::{burn, error::SpamError};
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
    jsonrpc::{self, Client as JsonClient},
    Client, RpcApi,
};
use serde_json::json;
use simchain_common::live_tuning::SpamTuning;
use simchain_common::rpc_retry;
use std::collections::HashSet;

const FLOOR_BATCH_SIZE: usize = 250;

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

// vsize of one floor fill: a 1-in/1-out P2WPKH self-transfer (11 vB overhead
// + 68 vB input + 31 vB change). The smallest SELF-SUSTAINING standard shape:
// the change must be spendable as the next fill's input, and a taproot
// self-transfer is bigger (~111 vB: 57.5 vB input but a 43 vB output).
const FILL_VSIZE: u64 = 110;
// In DATA/HYBRID mode, bulk spam pays a tiny premium over the floor fills so
// block assembly drains bulk weight first and uses floor fills only to seal
// residual gaps. The visible floor still comes from fills at SPAM_FEE.
const POOL_FANOUT_CHUNK_OUTPUTS: usize = 500;
// Fan-out/refill transactions must confirm even when floor-priced spam fills
// every block. Paying above the floor keeps the refill path from competing
// with the traffic it is trying to replenish.

// One funding pull for the floor pool. Kept modest because fills only burn
// fees and recycle their change 1:1 after confirmation.
const POOL_PULL_MAX_BTC: f64 = 50.0;

#[derive(Clone, Copy)]
struct Utxo {
    outpoint: OutPoint,
    amount: Amount,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoincore_rpc::Auth;
    use simchain_common::live_tuning;
    use std::collections::BTreeMap;

    fn rpc_client() -> Client {
        Client::new("http://127.0.0.1:1", Auth::None).expect("test RPC client")
    }

    fn json_client() -> JsonClient {
        let transport = jsonrpc::simple_http::SimpleHttpTransport::builder()
            .url("http://127.0.0.1:1")
            .expect("test RPC URL")
            .build();
        JsonClient::with_transport(transport)
    }

    fn policy() -> SpamTuning {
        SpamTuning::from_source(&live_tuning::staged_map(&BTreeMap::new()))
            .expect("default spam policy")
            .0
    }

    fn utxo(tag: u8, amount: u64) -> Utxo {
        Utxo {
            outpoint: OutPoint::new(Txid::from_byte_array([tag; 32]), 0),
            amount: Amount::from_sat(amount),
        }
    }

    #[test]
    fn applying_shape_policy_preserves_dynamic_engine_state() {
        let initial = policy();
        let mut engine = RawSpammer::new(
            rpc_client(),
            json_client(),
            Vec::new(),
            rpc_client(),
            "wallet",
            "test",
            "Test",
            initial.fee_rate_sat_vb(),
            initial.sendmany_outputs,
            initial.effective_data_min_bytes(),
            initial.data_max_bytes,
        );
        let branch = Utxo {
            outpoint: OutPoint::new(Txid::all_zeros(), 1),
            amount: Amount::from_btc(1.0).expect("amount"),
        };
        let pool = Utxo {
            outpoint: OutPoint::new(Txid::all_zeros(), 2),
            amount: Amount::from_btc(0.5).expect("amount"),
        };
        engine.utxos.push(branch);
        engine.pool_utxos.push(pool);
        engine.fills_inflight.push(pool);
        engine.cursor = 7;
        engine.draw_counter = 42;
        engine.pool_seen_height = 99;

        let mut changed = initial;
        changed.spam_fee *= 2.0;
        changed.sendmany_outputs = 3;
        changed.data_min_bytes = 100;
        changed.data_max_bytes = 1_000;
        let prepared = RawSpammer::prepare_policy(&changed);
        engine.apply_prepared_policy(&prepared);

        assert_eq!(engine.utxos[0].outpoint, branch.outpoint);
        assert_eq!(engine.pool_utxos[0].outpoint, pool.outpoint);
        assert_eq!(engine.fills_inflight[0].outpoint, pool.outpoint);
        assert_eq!(engine.cursor, 7);
        assert_eq!(engine.draw_counter, 42);
        assert_eq!(engine.pool_seen_height, 99);
        assert_eq!(engine.fee_rate_sat_vb, changed.fee_rate_sat_vb());
        assert_eq!(engine.burn_scripts.len(), 3);
        assert_eq!(engine.data_min, 100);
        assert_eq!(engine.data_max, 1_000);
    }

    #[test]
    fn ratio_increase_uses_headroom_while_requesting_background_fanout() {
        assert_eq!(
            branch_provisioning_action(60, 50, 75, false, false),
            BranchProvisioningAction::StartFanout
        );

        let branches: Vec<Utxo> = (0..60)
            .map(|index| utxo(index, 10_000 + u64::from(index)))
            .collect();
        let candidates = branch_fanout_candidates(&branches, Amount::from_sat(1_000), 50, None);
        assert_eq!(candidates.len(), 60);
        assert_eq!(candidates[0], 59);
    }

    #[test]
    fn degraded_capacity_funds_without_consuming_an_active_branch() {
        assert_eq!(
            branch_provisioning_action(40, 50, 75, false, false),
            BranchProvisioningAction::StartFunding
        );

        let mut branches: Vec<Utxo> = (0..40)
            .map(|index| utxo(index, 10_000 + u64::from(index)))
            .collect();
        assert!(branch_fanout_candidates(&branches, Amount::from_sat(1_000), 50, None,).is_empty());

        let funding_seed = utxo(100, 1_000_000);
        branches.push(funding_seed);
        assert_eq!(
            branch_provisioning_action(41, 50, 75, false, true),
            BranchProvisioningAction::StartFanout
        );
        assert_eq!(
            branch_fanout_candidates(
                &branches,
                Amount::from_sat(1_000),
                50,
                Some(funding_seed.outpoint),
            ),
            vec![40]
        );
    }

    #[test]
    fn minimum_capacity_is_not_spent_to_reach_the_preferred_target() {
        assert_eq!(
            branch_provisioning_action(50, 50, 75, false, false),
            BranchProvisioningAction::StartFunding
        );
        let branches: Vec<Utxo> = (0..50)
            .map(|index| utxo(index, 10_000 + u64::from(index)))
            .collect();
        assert!(branch_fanout_candidates(&branches, Amount::from_sat(1_000), 50, None,).is_empty());
    }
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

struct PendingFunding {
    txid: Txid,
    output: Option<Utxo>,
}

struct PendingFanout {
    outputs: Vec<Utxo>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BranchProvisioningAction {
    Ready,
    WaitForFanout,
    StartFanout,
    StartFunding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawEngineCapacity {
    pub usable_branches: u64,
    pub branch_provisioning: bool,
    pub floor_pool_provisioning: bool,
}

pub struct RawSpammer {
    node: Client,
    node_batch: JsonClient,
    // Extra nodes that receive floor-fill txs directly after the owner node
    // accepts them. Floor fills are the only txs that must be in both rotating
    // miners' local mempools before the next template is assembled.
    relay_nodes: Vec<JsonClient>,
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
    branch_funding: Option<PendingFunding>,
    branch_fanout: Option<PendingFanout>,
    // Floor fill pool: a SECOND deterministic key whose confirmed UTXOs feed
    // the standalone floor fills. Separate from the data branches so the
    // fills never chain off unconfirmed spam change and stay identifiable.
    pool_secret: SecretKey,
    pool_pubkey: PublicKey,
    pool_address: Address,
    pool_script: ScriptBuf,
    // Confirmed pool UTXOs ready to be spent as fills ("ammo").
    pool_utxos: Vec<Utxo>,
    // Fills currently sitting unmined in the mempool (the standing pool).
    // Each entry is the fill's own change output: outpoint (txid, 0) carries
    // the fill txid for mined-detection, and the amount is ready to move to
    // pool_utxos the moment a block mines it.
    fills_inflight: Vec<Utxo>,
    pool_funding: Option<PendingFunding>,
    pool_fanout: Option<PendingFanout>,
    // Last block height whose txs were checked for mined fills, so a cycle
    // that overruns a block interval never misses a mined fill.
    pool_seen_height: u64,
}

#[derive(Clone)]
pub struct PreparedRawPolicy {
    fee_rate_sat_vb: f64,
    burn_scripts: Vec<ScriptBuf>,
    data_min: usize,
    data_max: usize,
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

fn branch_provisioning_action(
    usable: u64,
    minimum: u64,
    target: u64,
    fanout_pending: bool,
    confirmed_funding_seed: bool,
) -> BranchProvisioningAction {
    if usable >= target {
        BranchProvisioningAction::Ready
    } else if fanout_pending {
        BranchProvisioningAction::WaitForFanout
    } else if confirmed_funding_seed || usable > minimum {
        BranchProvisioningAction::StartFanout
    } else {
        BranchProvisioningAction::StartFunding
    }
}

fn branch_fanout_candidates(
    utxos: &[Utxo],
    required: Amount,
    minimum: u64,
    preferred_seed: Option<OutPoint>,
) -> Vec<usize> {
    let usable = utxos.iter().filter(|utxo| utxo.amount >= required).count() as u64;
    let preferred = preferred_seed.and_then(|outpoint| {
        utxos
            .iter()
            .position(|utxo| utxo.outpoint == outpoint && utxo.amount >= required)
    });
    let mut candidates = Vec::new();
    if let Some(index) = preferred {
        candidates.push(index);
    }
    if usable > minimum {
        let mut remaining: Vec<usize> = (0..utxos.len())
            .filter(|index| Some(*index) != preferred && utxos[*index].amount >= required)
            .collect();
        remaining.sort_by_key(|index| std::cmp::Reverse(utxos[*index].amount.to_sat()));
        candidates.extend(remaining);
    }
    candidates
}

impl RawSpammer {
    pub fn prepare_policy(policy: &SpamTuning) -> PreparedRawPolicy {
        let burn_scripts = if policy.sendmany_outputs == 0 {
            vec![burn::burn_address(0).script_pubkey()]
        } else {
            (1..=policy.sendmany_outputs)
                .map(|i| burn::burn_address(i).script_pubkey())
                .collect()
        };
        PreparedRawPolicy {
            fee_rate_sat_vb: policy.fee_rate_sat_vb(),
            burn_scripts,
            data_min: policy.effective_data_min_bytes() as usize,
            data_max: policy.data_max_bytes as usize,
        }
    }

    pub fn apply_prepared_policy(&mut self, policy: &PreparedRawPolicy) {
        self.fee_rate_sat_vb = policy.fee_rate_sat_vb;
        self.burn_scripts.clone_from(&policy.burn_scripts);
        self.data_min = policy.data_min;
        self.data_max = policy.data_max;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        node: Client,
        node_batch: JsonClient,
        relay_nodes: Vec<JsonClient>,
        wallet: Client,
        wallet_name: &str,
        key_namespace: &str,
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
        // The namespace keeps independent engine instances (the resident
        // spammer vs a control-plane scenario burst) on disjoint keys so they
        // never track and double-spend the same UTXO set.
        let secp = Secp256k1::new();
        let tag = sha256::Hash::hash(format!("simchain-raw-spam-{key_namespace}").as_bytes());
        let secret =
            SecretKey::from_slice(tag.as_byte_array()).expect("sha256 of tag is a valid key");
        let pubkey = PublicKey::from_secret_key(&secp, &secret);
        let address = Address::p2wpkh(&CompressedPublicKey(pubkey), Network::Regtest);
        let script_pubkey = address.script_pubkey();
        let burn_scripts: Vec<ScriptBuf> = if sendmany_outputs == 0 {
            vec![burn::burn_address(0).script_pubkey()]
        } else {
            (1..=sendmany_outputs)
                .map(|i| burn::burn_address(i).script_pubkey())
                .collect()
        };
        let sealer_script = burn::burn_address(0).script_pubkey();
        // The floor pool's own key, same recovery story as the engine key: a
        // restarted spammer derives the same address and picks its confirmed
        // pool UTXOs back up with scantxoutset.
        let pool_tag = sha256::Hash::hash(format!("simchain-raw-floor-{key_namespace}").as_bytes());
        let pool_secret =
            SecretKey::from_slice(pool_tag.as_byte_array()).expect("sha256 of tag is a valid key");
        let pool_pubkey = PublicKey::from_secret_key(&secp, &pool_secret);
        let pool_address = Address::p2wpkh(&CompressedPublicKey(pool_pubkey), Network::Regtest);
        let pool_script = pool_address.script_pubkey();
        tracing::info!("{label} => Raw spam engine address: {address}");
        tracing::info!("{label} => Floor fill-pool address: {pool_address}");
        if !relay_nodes.is_empty() {
            tracing::info!(
                "{label} => Direct floor-fill RPC relay peers: {}",
                relay_nodes.len()
            );
        }
        RawSpammer {
            node,
            node_batch,
            relay_nodes,
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
            branch_funding: None,
            branch_fanout: None,
            pool_secret,
            pool_pubkey,
            pool_address,
            pool_script,
            pool_utxos: Vec::new(),
            fills_inflight: Vec::new(),
            pool_funding: None,
            pool_fanout: None,
            pool_seen_height: 0,
        }
    }

    fn fee_from_vsize(&self, vsize: u64) -> Amount {
        Amount::from_sat((vsize as f64 * self.fee_rate_sat_vb).ceil() as u64)
    }

    fn bulk_fee_from_vsize(&self, vsize: u64) -> Amount {
        Amount::from_sat(
            (vsize as f64 * (self.fee_rate_sat_vb + SpamTuning::BULK_FEE_PREMIUM_SAT_VB)).ceil()
                as u64,
        )
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
        let vsize = self.shape_vsize(shape);
        if self.data_max > 0 {
            self.bulk_fee_from_vsize(vsize)
        } else {
            self.fee_from_vsize(vsize)
        }
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
    // outputs, no data. It deliberately pays above the simulated floor so
    // refill transactions confirm promptly under saturation.
    fn consolidation_fee(&self, n_in: usize, n_out: usize) -> Amount {
        self.fee_from_vsize((11 + 68 * n_in + 31 * n_out) as u64)
            * SpamTuning::FANOUT_FEE_MULTIPLIER
    }

    // Build, sign and broadcast one transaction spending the engine key's
    // P2WPKH UTXOs. maxfeerate=0 disables sendrawtransaction's 0.1 BTC/kvB
    // safety cap, so a deliberately high SPAM_FEE price level still
    // broadcasts.
    fn send_tx(
        &self,
        inputs: &[Utxo],
        outputs: Vec<TxOut>,
        replaceable: bool,
    ) -> Result<Txid, bitcoincore_rpc::Error> {
        self.send_signed(
            inputs,
            outputs,
            replaceable,
            &self.script_pubkey,
            &self.secret,
            &self.pubkey,
        )
    }

    // send_tx generalized over the spending key, so the same signer serves
    // both the engine key (data branches) and the floor-pool key (fills).
    // All inputs must pay `spent_script` (P2WPKH of `pubkey`).
    fn signed_tx(
        &self,
        inputs: &[Utxo],
        outputs: Vec<TxOut>,
        replaceable: bool,
        spent_script: &ScriptBuf,
        secret: &SecretKey,
        pubkey: &PublicKey,
    ) -> Transaction {
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
                .p2wpkh_signature_hash(i, spent_script, utxo.amount, EcdsaSighashType::All)
                .expect("valid p2wpkh sighash");
            let signature = ecdsa::Signature {
                signature: self
                    .secp
                    .sign_ecdsa(&Message::from_digest(sighash.to_byte_array()), secret),
                sighash_type: EcdsaSighashType::All,
            };
            *cache.witness_mut(i).unwrap() = Witness::p2wpkh(&signature, pubkey);
        }
        tx
    }

    fn send_signed(
        &self,
        inputs: &[Utxo],
        outputs: Vec<TxOut>,
        replaceable: bool,
        spent_script: &ScriptBuf,
        secret: &SecretKey,
        pubkey: &PublicKey,
    ) -> Result<Txid, bitcoincore_rpc::Error> {
        let raw_tx = serialize_hex(&self.signed_tx(
            inputs,
            outputs,
            replaceable,
            spent_script,
            secret,
            pubkey,
        ));
        self.node
            .call::<String>("sendrawtransaction", &[json!(&raw_tx), json!(0)])
            .map(|s| s.parse().expect("bitcoind returned an invalid txid"))
    }

    fn send_raw_batch(client: &JsonClient, raw_txs: &[String]) -> Vec<Result<Txid, SpamError>> {
        if raw_txs.is_empty() {
            return Vec::new();
        }
        let params: Vec<_> = raw_txs
            .iter()
            .map(|raw| jsonrpc::arg(vec![json!(raw), json!(0)]))
            .collect();
        let requests: Vec<_> = params
            .iter()
            .map(|p| client.build_request("sendrawtransaction", Some(p.as_ref())))
            .collect();

        match client.send_batch(&requests) {
            Ok(responses) => responses
                .into_iter()
                .map(|response| {
                    let response = response.ok_or(SpamError::MissingBatchResponse)?;
                    let txid = response
                        .result::<String>()
                        .map_err(|e| SpamError::Rpc(e.to_string()))?;
                    txid.parse().map_err(|_| SpamError::InvalidTxid)
                })
                .collect(),
            Err(e) => {
                let msg = e.to_string();
                raw_txs
                    .iter()
                    .map(|_| Err(SpamError::Rpc(msg.clone())))
                    .collect()
            }
        }
    }

    fn relay_raw_batch(&self, raw_txs: &[String]) {
        for relay_node in &self.relay_nodes {
            let _ = Self::send_raw_batch(relay_node, raw_txs);
        }
    }

    // Confirmed, still-unspent UTXOs paying `address`, from the chain.
    // scantxoutset only sees CONFIRMED outputs, so two corrections apply:
    // outputs already spent by our own still-in-mempool txs are filtered out
    // with gettxout(include_mempool), and outputs whose tx is unconfirmed
    // stay invisible until a block confirms them (the next low-pool check
    // picks them back up). Only a recovery path -- startup, reorgs, lost
    // track -- never the hot path.
    fn scan_address_utxos(&self, address: &Address) -> Vec<Utxo> {
        let scan = rpc_retry("scan address UTXOs", || {
            self.node
                .scan_tx_out_set_blocking(&[ScanTxOutRequest::Single(format!("addr({address})"))])
        });
        scan.unspents
            .into_iter()
            .filter(|u| {
                rpc_retry("check scanned UTXO", || {
                    self.node.get_tx_out(&u.txid, u.vout, Some(true))
                })
                .is_some()
            })
            .map(|u| Utxo {
                outpoint: OutPoint::new(u.txid, u.vout),
                amount: u.amount,
            })
            .collect()
    }

    fn try_scan_address_utxos(&self, address: &Address) -> anyhow::Result<Vec<Utxo>> {
        let scan = self
            .node
            .scan_tx_out_set_blocking(&[ScanTxOutRequest::Single(format!("addr({address})"))])?;
        let mut utxos = Vec::new();
        for unspent in scan.unspents {
            if self
                .node
                .get_tx_out(&unspent.txid, unspent.vout, Some(true))?
                .is_some()
            {
                utxos.push(Utxo {
                    outpoint: OutPoint::new(unspent.txid, unspent.vout),
                    amount: unspent.amount,
                });
            }
        }
        Ok(utxos)
    }

    fn funding_output(&self, txid: Txid, script: &ScriptBuf) -> Option<Utxo> {
        self.node
            .get_raw_transaction(&txid, None)
            .ok()?
            .output
            .into_iter()
            .enumerate()
            .find(|(_, output)| &output.script_pubkey == script)
            .map(|(vout, output)| Utxo {
                outpoint: OutPoint::new(txid, vout as u32),
                amount: output.value,
            })
    }

    fn funding_confirmed(&self, pending: &PendingFunding) -> bool {
        self.wallet
            .get_transaction(&pending.txid, None)
            .map(|tx| tx.info.confirmations >= 1)
            .unwrap_or(false)
    }

    fn funding_alive(&self, pending: &PendingFunding) -> bool {
        self.wallet
            .get_transaction(&pending.txid, None)
            .map(|tx| tx.info.confirmations >= 0)
            .unwrap_or(false)
    }

    fn fanout_confirmed(&self, pending: &PendingFanout) -> bool {
        pending.outputs.first().is_some_and(|output| {
            matches!(
                self.node
                    .get_tx_out(&output.outpoint.txid, output.outpoint.vout, Some(false)),
                Ok(Some(_))
            )
        })
    }

    fn fanout_alive(&self, pending: &PendingFanout) -> bool {
        pending.outputs.first().is_some_and(|output| {
            matches!(
                self.node
                    .get_tx_out(&output.outpoint.txid, output.outpoint.vout, Some(true)),
                Ok(Some(_))
            )
        })
    }

    /// Rebuild mutable branch and floor-pool state after a chain mutation or
    /// before atomically installing a replacement engine.
    pub fn reconcile(&mut self) -> anyhow::Result<()> {
        let utxos = self.try_scan_address_utxos(&self.address)?;
        let pool_utxos = self.try_scan_address_utxos(&self.pool_address)?;
        let mut fills_inflight = Vec::new();
        for fill in &self.fills_inflight {
            if self
                .node
                .get_tx_out(&fill.outpoint.txid, fill.outpoint.vout, Some(true))?
                .is_some()
                && !pool_utxos.iter().any(|utxo| utxo.outpoint == fill.outpoint)
            {
                fills_inflight.push(*fill);
            }
        }
        let branch_funding = self
            .branch_funding
            .take()
            .filter(|pending| self.funding_alive(pending));
        let branch_fanout = self
            .branch_fanout
            .take()
            .filter(|pending| self.fanout_alive(pending));
        let pool_funding = self
            .pool_funding
            .take()
            .filter(|pending| self.funding_alive(pending));
        let pool_fanout = self
            .pool_fanout
            .take()
            .filter(|pending| self.fanout_alive(pending));
        self.utxos = utxos;
        self.cursor = 0;
        self.branch_funding = branch_funding;
        self.branch_fanout = branch_fanout;
        self.pool_utxos = pool_utxos;
        self.fills_inflight = fills_inflight;
        self.pool_funding = pool_funding;
        self.pool_fanout = pool_fanout;
        self.pool_seen_height = self.node.get_block_count()?;
        Ok(())
    }

    /// Number of confirmed branches that can pay one transaction with the
    /// current fee rate and burn-output shape.
    pub fn usable_branches_for_current_shape(&self) -> u64 {
        self.usable_branches(self.per_tx_required())
    }

    pub fn capacity(&self) -> RawEngineCapacity {
        RawEngineCapacity {
            usable_branches: self.usable_branches_for_current_shape(),
            branch_provisioning: self.branch_funding.is_some() || self.branch_fanout.is_some(),
            floor_pool_provisioning: self.pool_funding.is_some() || self.pool_fanout.is_some(),
        }
    }

    /// Make sure `branches` confirmed, usable branch UTXOs exist for the
    /// current fee rate and burn-output shape, starting wallet funding and
    /// fan-out when needed. Confirmation progresses between calls, so it needs
    /// block production to recover a broke engine without blocking the worker.
    /// Returns false when interrupted or when the requested confirmed branches
    /// are not available yet.
    pub fn ensure_branches(&mut self, branches: u64, checkpoint: &impl Fn(&str) -> bool) -> bool {
        let branches = branches.max(1);
        if !self.ensure_funds(branches, branches, checkpoint) {
            return false;
        }
        self.usable_branches_for_current_shape() >= branches
    }

    /// Retarget the OUTPUT-mode burn shape and fee rate for the next round,
    /// so an owner can reuse one engine instance (and its in-memory branch
    /// state) across differently-shaped bursts.
    pub fn set_burst_shape(&mut self, fee_rate_sat_vb: f64, sendmany_outputs: u64) {
        self.fee_rate_sat_vb = fee_rate_sat_vb;
        self.data_min = 0;
        self.data_max = 0;
        self.burn_scripts = if sendmany_outputs == 0 {
            vec![burn::burn_address(0).script_pubkey()]
        } else {
            (1..=sendmany_outputs)
                .map(|i| burn::burn_address(i).script_pubkey())
                .collect()
        };
    }

    /// Retarget the burst engine to fixed-size OP_RETURN DATA transactions.
    pub fn set_burst_data_shape(&mut self, fee_rate_sat_vb: f64, data_bytes: u64) {
        self.fee_rate_sat_vb = fee_rate_sat_vb;
        self.data_min = data_bytes as usize;
        self.data_max = data_bytes as usize;
    }

    // Rebuild the data-branch UTXO set from the chain.
    fn resync(&mut self) {
        self.utxos = self.scan_address_utxos(&self.address);
        self.cursor = 0;
    }

    // Rebuild the floor pool's confirmed ammo from the chain.
    fn pool_resync(&mut self) {
        self.pool_utxos = self.scan_address_utxos(&self.pool_address);
    }

    fn advance_branch_provisioning(&mut self) -> Option<OutPoint> {
        let mut confirmed_funding_seed = None;
        if self
            .branch_funding
            .as_ref()
            .is_some_and(|pending| self.funding_confirmed(pending))
        {
            let pending = self.branch_funding.take().expect("checked above");
            match pending.output {
                Some(output) => {
                    confirmed_funding_seed = Some(output.outpoint);
                    self.utxos.push(output);
                }
                None => self.resync(),
            }
            tracing::info!("{} => Raw engine funding confirmed", self.label);
        }
        if self
            .branch_fanout
            .as_ref()
            .is_some_and(|pending| self.fanout_confirmed(pending))
        {
            let pending = self.branch_fanout.take().expect("checked above");
            self.utxos.extend(pending.outputs);
            if !self.utxos.is_empty() {
                self.cursor %= self.utxos.len();
            }
            tracing::info!("{} => Background fan-out confirmed", self.label);
        }
        confirmed_funding_seed
    }

    fn start_branch_funding(&mut self) {
        if self.branch_funding.is_some() {
            return;
        }
        let Ok(balances) = self.wallet.get_balances() else {
            return;
        };
        let trusted = balances.mine.trusted.to_btc();
        if trusted < 1.0 {
            return;
        }
        let pull_btc = ((trusted * 0.5).min(FUND_PULL_MAX_BTC) * 1e8).floor() / 1e8;
        let pull = Amount::from_btc(pull_btc).expect("rounded BTC amount");
        tracing::info!(
            "{} => Raw engine pulling {pull} from wallet '{}' in the background",
            self.label,
            self.wallet_name
        );
        match self
            .wallet
            .send_to_address(&self.address, pull, None, None, None, None, None, None)
        {
            Ok(txid) => {
                self.branch_funding = Some(PendingFunding {
                    txid,
                    output: self.funding_output(txid, &self.script_pubkey),
                });
            }
            Err(error) => tracing::warn!(
                "{} => Raw engine funding pull failed ({error}), deferring until the next block",
                self.label
            ),
        }
    }

    fn start_branch_fanout(
        &mut self,
        minimum: u64,
        target: u64,
        required: Amount,
        preferred_seed: Option<OutPoint>,
    ) -> bool {
        if self.branch_fanout.is_some() || self.utxos.is_empty() {
            return false;
        }
        let usable = self.usable_branches(required);
        if usable >= target {
            return false;
        }
        let output_count = target.saturating_sub(usable).saturating_add(1) as usize;
        let Some(index) = branch_fanout_candidates(&self.utxos, required, minimum, preferred_seed)
            .into_iter()
            .find(|index| {
                matches!(
                    self.node.get_tx_out(
                        &self.utxos[*index].outpoint.txid,
                        self.utxos[*index].outpoint.vout,
                        Some(false),
                    ),
                    Ok(Some(_))
                )
            })
        else {
            return false;
        };
        let input = self.utxos[index];
        let fee = self.consolidation_fee(1, output_count);
        let Some(split) = input.amount.checked_sub(fee) else {
            return false;
        };
        let per_branch = split / output_count as u64;
        if per_branch < required * BRANCH_MIN_TXS {
            return false;
        }
        let outputs = (0..output_count)
            .map(|_| TxOut {
                value: per_branch,
                script_pubkey: self.script_pubkey.clone(),
            })
            .collect();
        match self.send_tx(&[input], outputs, false) {
            Ok(txid) => {
                self.utxos.remove(index);
                self.branch_fanout = Some(PendingFanout {
                    outputs: (0..output_count)
                        .map(|vout| Utxo {
                            outpoint: OutPoint::new(txid, vout as u32),
                            amount: per_branch,
                        })
                        .collect(),
                });
                if !self.utxos.is_empty() {
                    self.cursor %= self.utxos.len();
                }
                tracing::info!(
                    "{} => Background fan-out {txid} submitted for {output_count} branches",
                    self.label
                );
                true
            }
            Err(error) => {
                tracing::warn!(
                    "{} => Background fan-out failed ({error}), deferring until the next block",
                    self.label
                );
                false
            }
        }
    }

    // Keep at least `need` independent branches usable while moving toward the
    // preferred `target`. Expansion is submitted in the background and never
    // waits for a confirmation in the spam cycle.
    fn ensure_funds(&mut self, need: u64, target: u64, checkpoint: &impl Fn(&str) -> bool) -> bool {
        if !checkpoint("raw_funds_check") {
            return false;
        }
        let confirmed_funding_seed = self.advance_branch_provisioning();
        if self.utxos.is_empty() && self.branch_funding.is_none() && self.branch_fanout.is_none() {
            self.resync();
        }
        let required = self.per_tx_required();
        let usable = self.usable_branches(required);
        let total: Amount = self.utxos.iter().map(|u| u.amount).sum();
        let refill_floor = required * (target * BRANCH_MIN_TXS);
        if total < refill_floor {
            self.start_branch_funding();
        }
        match branch_provisioning_action(
            usable,
            need,
            target,
            self.branch_fanout.is_some(),
            confirmed_funding_seed.is_some(),
        ) {
            BranchProvisioningAction::StartFanout => {
                if !self.start_branch_fanout(need, target, required, confirmed_funding_seed) {
                    self.start_branch_funding();
                }
            }
            BranchProvisioningAction::StartFunding => self.start_branch_funding(),
            BranchProvisioningAction::Ready | BranchProvisioningAction::WaitForFanout => {}
        }
        usable > 0
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
    // the SentSpam record (for RBF) or a classified error.
    fn send_shape(&mut self, shape: SpamShape, replaceable: bool) -> Result<SentSpam, SpamError> {
        let required = self.per_tx_required();
        let Some(idx) = self.next_branch(required) else {
            return Err(SpamError::NoUsableBranch);
        };
        let branch = self.utxos[idx];
        let fee = self.shape_fee(&shape);
        let nonchange = self.shape_nonchange_value(&shape);
        let change = match branch.amount.checked_sub(nonchange + fee) {
            Some(c) if c >= MIN_CHANGE => c,
            _ => return Err(SpamError::BranchTooSmall),
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
                Err(SpamError::Rpc(msg))
            }
        }
    }

    // Fee-bump (RBF) up to `count` of the just-sent spam txs: rebuild the same
    // spend (same shape) with double the fee (change shrinks by the old fee,
    // clearing BIP125's +1 sat/vB minimum), re-sign, broadcast. Only branch
    // TIPS can be replaced -- if a later tx already chained off this one's
    // change, replacing it would orphan that child -- and the tip check is
    // simply "is this tx's change outpoint still in our UTXO set".
    fn bump_spam_txs(&mut self, sent: &[SentSpam], count: u64, checkpoint: &impl Fn(&str) -> bool) {
        let mut bumped = 0;
        let mut first_error: Option<String> = None;
        for s in sent.iter().rev() {
            if bumped >= count || !checkpoint("raw_rbf_before_submit") {
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
            if !checkpoint("raw_rbf_after_submit") {
                break;
            }
        }
        match first_error {
            Some(error) if bumped < count => tracing::info!(
                "{} => Fee-bumped (RBF) {bumped}/{count} raw spam txs, first error: {error}",
                self.label
            ),
            _ => tracing::info!("{} => Fee-bumped (RBF) {bumped} raw spam txs", self.label),
        }
    }

    fn fill_fee(&self) -> Amount {
        self.fee_from_vsize(FILL_VSIZE)
    }

    // Move fills that a block has mined from the in-flight list back into the
    // pool's confirmed ammo (a mined fill's change is spendable again). Walks
    // every block since the last look, so fills mined while a spam cycle
    // overran a block interval are never missed and the standing count stays
    // honest.
    fn harvest_mined_fills(&mut self) {
        let tip = rpc_retry("get floor-pool block count", || self.node.get_block_count());
        if self.pool_seen_height == 0 || self.fills_inflight.is_empty() {
            self.pool_seen_height = tip;
            return;
        }
        let mut mined: HashSet<Txid> = HashSet::new();
        for height in (self.pool_seen_height + 1)..=tip {
            let hash = match self.node.get_block_hash(height) {
                Ok(hash) => hash,
                Err(error) => {
                    tracing::warn!(
                        "{} => Floor-pool harvest skipped at height {height}: block hash RPC failed ({error})",
                        self.label
                    );
                    return;
                }
            };
            let block = match self.node.get_block_info(&hash) {
                Ok(block) => block,
                Err(error) => {
                    tracing::warn!(
                        "{} => Floor-pool harvest skipped for block {hash}: block info RPC failed ({error})",
                        self.label
                    );
                    return;
                }
            };
            mined.extend(block.tx);
        }
        self.pool_seen_height = tip;
        let mut still_standing = Vec::new();
        for fill in self.fills_inflight.drain(..) {
            // The duplicate guard covers the rare race where a resync already
            // picked this change up as confirmed ammo before we saw the block.
            if mined.contains(&fill.outpoint.txid) {
                if !self.pool_utxos.iter().any(|u| u.outpoint == fill.outpoint) {
                    self.pool_utxos.push(fill);
                }
            } else {
                still_standing.push(fill);
            }
        }
        self.fills_inflight = still_standing;
    }

    fn usable_fill_ammo(&self, required: Amount) -> u64 {
        self.pool_utxos
            .iter()
            .filter(|u| u.amount >= required)
            .count() as u64
    }

    fn advance_pool_provisioning(&mut self) {
        if self
            .pool_funding
            .as_ref()
            .is_some_and(|pending| self.funding_confirmed(pending))
        {
            let pending = self.pool_funding.take().expect("checked above");
            match pending.output {
                Some(output) => self.pool_utxos.push(output),
                None => self.pool_resync(),
            }
            tracing::info!("{} => Floor-pool funding confirmed", self.label);
        }
        if self
            .pool_fanout
            .as_ref()
            .is_some_and(|pending| self.fanout_confirmed(pending))
        {
            let pending = self.pool_fanout.take().expect("checked above");
            self.pool_utxos.extend(pending.outputs);
            tracing::info!("{} => Background floor-pool fan-out confirmed", self.label);
        }
    }

    fn start_pool_funding(&mut self) {
        if self.pool_funding.is_some() {
            return;
        }
        let Ok(balances) = self.wallet.get_balances() else {
            return;
        };
        let trusted = balances.mine.trusted.to_btc();
        if trusted < 1.0 {
            return;
        }
        let pull_btc = ((trusted * 0.5).min(POOL_PULL_MAX_BTC) * 1e8).floor() / 1e8;
        let pull = Amount::from_btc(pull_btc).expect("rounded BTC amount");
        tracing::info!(
            "{} => Floor pool pulling {pull} from wallet '{}' in the background",
            self.label,
            self.wallet_name
        );
        match self.wallet.send_to_address(
            &self.pool_address,
            pull,
            None,
            None,
            None,
            None,
            None,
            None,
        ) {
            Ok(txid) => {
                self.pool_funding = Some(PendingFunding {
                    txid,
                    output: self.funding_output(txid, &self.pool_script),
                });
            }
            Err(error) => tracing::warn!(
                "{} => Floor-pool funding pull failed ({error}), deferring until the next block",
                self.label
            ),
        }
    }

    fn start_pool_fanout(&mut self, seed_count: u64, required: Amount) -> bool {
        if self.pool_fanout.is_some() || self.pool_utxos.is_empty() {
            return false;
        }
        let usable = self.usable_fill_ammo(required);
        if usable >= seed_count {
            return false;
        }
        let n_fill = seed_count
            .saturating_sub(usable)
            .min(POOL_FANOUT_CHUNK_OUTPUTS as u64) as usize;
        let per_utxo = required * BRANCH_MIN_TXS;
        let fill_value = per_utxo * n_fill as u64;

        let mut candidates: Vec<usize> = (0..self.pool_utxos.len()).collect();
        candidates.sort_by_key(|index| self.pool_utxos[*index].amount.to_sat());
        let mut selected = Vec::new();
        let mut input_total = Amount::ZERO;
        while input_total < fill_value + self.consolidation_fee(selected.len().max(1), n_fill + 1) {
            let Some(index) = candidates.pop() else {
                return false;
            };
            input_total += self.pool_utxos[index].amount;
            selected.push(index);
        }
        selected.sort_unstable_by(|left, right| right.cmp(left));
        let inputs: Vec<Utxo> = selected
            .into_iter()
            .map(|index| self.pool_utxos.swap_remove(index))
            .collect();
        let fee = self.consolidation_fee(inputs.len(), n_fill + 1);
        let change = input_total
            .checked_sub(fill_value + fee)
            .unwrap_or(Amount::ZERO);
        let include_change = change >= required;
        let mut outputs: Vec<TxOut> = (0..n_fill)
            .map(|_| TxOut {
                value: per_utxo,
                script_pubkey: self.pool_script.clone(),
            })
            .collect();
        if include_change {
            outputs.push(TxOut {
                value: change,
                script_pubkey: self.pool_script.clone(),
            });
        }
        match self.send_signed(
            &inputs,
            outputs,
            false,
            &self.pool_script,
            &self.pool_secret,
            &self.pool_pubkey,
        ) {
            Ok(txid) => {
                let mut pending_outputs: Vec<Utxo> = (0..n_fill)
                    .map(|vout| Utxo {
                        outpoint: OutPoint::new(txid, vout as u32),
                        amount: per_utxo,
                    })
                    .collect();
                if include_change {
                    pending_outputs.push(Utxo {
                        outpoint: OutPoint::new(txid, n_fill as u32),
                        amount: change,
                    });
                }
                self.pool_fanout = Some(PendingFanout {
                    outputs: pending_outputs,
                });
                tracing::info!(
                    "{} => Background floor-pool fan-out {txid} submitted for {n_fill} fill outputs",
                    self.label
                );
                true
            }
            Err(error) => {
                self.pool_utxos.extend(inputs);
                tracing::warn!(
                    "{} => Floor-pool fan-out failed ({error}), deferring until the next block",
                    self.label
                );
                false
            }
        }
    }

    // Keep enough confirmed floor ammo to top up the standing target. Funding
    // and fan-out confirmations advance between cycles, leaving existing ammo
    // available instead of blocking the worker.
    fn ensure_pool_funds(
        &mut self,
        need: u64,
        target: u64,
        checkpoint: &impl Fn(&str) -> bool,
    ) -> bool {
        if !checkpoint("floor_pool_funds_check") {
            return false;
        }
        self.advance_pool_provisioning();
        if self.pool_utxos.is_empty() && self.pool_funding.is_none() && self.pool_fanout.is_none() {
            self.pool_resync();
        }
        let required = self.fill_fee() + MIN_CHANGE;
        let usable = self.usable_fill_ammo(required);
        if usable >= need {
            return true;
        }
        let seed_count = target + target.div_ceil(4);
        let total: Amount = self.pool_utxos.iter().map(|u| u.amount).sum();
        let refill_floor = required * (seed_count * BRANCH_MIN_TXS);
        if total < refill_floor {
            self.start_pool_funding();
        }
        if self.pool_fanout.is_none() {
            self.start_pool_fanout(seed_count, required);
        }
        usable > 0
    }

    // Keep a standing pool of `target` standalone floor-priced fills sitting
    // in this node's mempool: harvest what the last block(s) mined (their
    // change is fresh confirmed ammo), then top the standing count back up.
    // Each fill spends one CONFIRMED pool UTXO, so its ancestor package is
    // itself and the block assembler can drop it into residual packing gaps.
    // Returns the number of fills sent.
    pub fn floor_round(&mut self, target: u64, checkpoint: &impl Fn(&str) -> bool) -> usize {
        if target == 0 {
            return 0;
        }
        if !checkpoint("floor_pool_harvest") {
            return 0;
        }
        self.harvest_mined_fills();
        let standing = self.fills_inflight.len() as u64;
        let need = target.saturating_sub(standing);
        if need == 0 {
            tracing::info!(
                "{} => Floor pool: {standing}/{target} fills standing, none needed",
                self.label
            );
            return 0;
        }
        if !self.ensure_pool_funds(need, target, checkpoint) {
            return 0;
        }

        let mut sent = 0u64;
        let mut attempted = 0u64;
        let mut sent_vsize = 0u64;
        let mut first_error: Option<String> = None;
        let required = self.fill_fee() + MIN_CHANGE;
        let fee = self.fill_fee();
        while attempted < need {
            if !checkpoint("floor_batch_prepare") {
                break;
            }
            let batch_target = ((need - attempted) as usize).min(FLOOR_BATCH_SIZE);
            let mut raw_fills: Vec<(String, Amount, Utxo)> = Vec::new();
            while raw_fills.len() < batch_target {
                let Some(idx) = self.pool_utxos.iter().position(|u| u.amount >= required) else {
                    break;
                };
                let utxo = self.pool_utxos.swap_remove(idx);
                let change = utxo.amount - fee;
                let outputs = vec![TxOut {
                    value: change,
                    script_pubkey: self.pool_script.clone(),
                }];
                let tx = self.signed_tx(
                    &[utxo],
                    outputs,
                    false,
                    &self.pool_script,
                    &self.pool_secret,
                    &self.pool_pubkey,
                );
                raw_fills.push((serialize_hex(&tx), change, utxo));
            }
            if raw_fills.is_empty() {
                break;
            }
            if !checkpoint("floor_batch_submit") {
                self.pool_utxos
                    .extend(raw_fills.into_iter().map(|(_, _, utxo)| utxo));
                break;
            }

            attempted += raw_fills.len() as u64;
            let raw_txs: Vec<String> = raw_fills.iter().map(|(raw, _, _)| raw.clone()).collect();
            let results = Self::send_raw_batch(&self.node_batch, &raw_txs);
            let mut relays = Vec::new();
            for ((raw_tx, change, _), result) in raw_fills.into_iter().zip(results) {
                match result {
                    Ok(txid) => {
                        self.fills_inflight.push(Utxo {
                            outpoint: OutPoint::new(txid, 0),
                            amount: change,
                        });
                        relays.push(raw_tx);
                        sent += 1;
                        sent_vsize += FILL_VSIZE;
                    }
                    Err(e) => {
                        // The UTXO stays dropped either way: if our view of it
                        // was stale, the next pool resync recovers the truth.
                        if first_error.is_none() {
                            first_error = Some(e.to_string());
                        }
                    }
                }
            }
            self.relay_raw_batch(&relays);
            if !checkpoint("floor_batch_submitted") {
                break;
            }
        }

        tracing::info!(
            "{} => Floor pool: {standing} standing + {sent} new fills (~{}k vB; target {target})",
            self.label,
            sent_vsize / 1000
        );
        if sent < need {
            let detail = first_error
                .map(|e| format!(", first error: {e}"))
                .unwrap_or_else(|| ", pool ammo exhausted".to_string());
            tracing::warn!(
                "{} floor pool only sent {sent}/{need} fills this block{detail}",
                self.label
            );
        }
        sent as usize
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
        checkpoint: &impl Fn(&str) -> bool,
    ) -> Vec<Txid> {
        if fanout > 0 && !self.ensure_funds(share.min(fanout), fanout, checkpoint) {
            return Vec::new();
        }
        let n_burns = self.burn_scripts.len();
        if n_burns == 1 {
            tracing::info!(
                "{} => Raw-spamming {share} transactions to a burn address",
                self.label
            );
        } else {
            tracing::info!(
                "{} => Raw-spamming {share} txs of {n_burns} outputs to burn addresses",
                self.label
            );
        }

        let mut txids = Vec::new();
        let mut sent = Vec::new();
        let mut first_error: Option<String> = None;
        let mut consecutive_failures = 0;
        while (txids.len() as u64) < share {
            if self.utxos.is_empty()
                || consecutive_failures >= self.utxos.len()
                || !checkpoint("raw_output_before_submit")
            {
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
                        first_error = Some(e.to_string());
                    }
                    consecutive_failures += 1;
                }
            }
            if !checkpoint("raw_output_after_submit") {
                break;
            }
        }
        if (txids.len() as u64) < share {
            let detail = first_error
                .map(|e| format!(", first error: {e}"))
                .unwrap_or_else(|| ", branch pool exhausted".to_string());
            tracing::warn!("only {}/{share} raw spam txs accepted{detail}", txids.len());
        }
        if replaceable {
            self.bump_spam_txs(&sent, replaces, checkpoint);
        }
        txids
    }

    // DATA burst: send a fixed number of OP_RETURN txs of the selected payload
    // size. This is the manual/scenario counterpart to hybrid_round's
    // block-filling data loop.
    pub fn data_round(
        &mut self,
        txs: u64,
        fanout: u64,
        data_bytes: u64,
        replaceable: bool,
        replaces: u64,
        checkpoint: &impl Fn(&str) -> bool,
    ) -> (Vec<Txid>, u64) {
        if fanout > 0 && !self.ensure_funds(txs.min(fanout), fanout, checkpoint) {
            return (Vec::new(), 0);
        }

        tracing::info!(
            "{} => Raw-spamming {txs} OP_RETURN data txs of {data_bytes} byte(s)",
            self.label
        );

        let mut txids = Vec::new();
        let mut sent = Vec::new();
        let mut added = 0u64;
        let mut first_error: Option<String> = None;
        let mut consecutive_failures = 0;
        let shape = SpamShape::Data(data_bytes as usize);
        while (txids.len() as u64) < txs {
            if self.utxos.is_empty()
                || consecutive_failures >= self.utxos.len()
                || !checkpoint("raw_data_burst_before_submit")
            {
                break;
            }
            match self.send_shape(shape.clone(), replaceable) {
                Ok(s) => {
                    added += self.shape_vsize(&s.shape);
                    txids.push(s.txid);
                    sent.push(s);
                    consecutive_failures = 0;
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                    consecutive_failures += 1;
                }
            }
            if !checkpoint("raw_data_burst_after_submit") {
                break;
            }
        }
        if (txids.len() as u64) < txs {
            let detail = first_error
                .map(|e| format!(", first error: {e}"))
                .unwrap_or_else(|| ", branch pool exhausted".to_string());
            tracing::warn!(
                "only {}/{txs} raw data spam txs accepted{detail}",
                txids.len()
            );
        }
        if replaceable {
            self.bump_spam_txs(&sent, replaces, checkpoint);
        }
        (txids, added)
    }

    // DATA/HYBRID mode: send `small_txs` minimum-size P2WPKH txs (cosmetic
    // small-payment-shaped traffic; the airtight floor is the separate
    // floor_round pool), then fill with varied-size OP_RETURN data txs until
    // `deficit_vsize` of weight has been offered (or the branch pool is
    // exhausted). `deficit_vsize` is how much the caller wants added to the
    // mempool this block to reach the SPAM_FILL_BLOCK_RATIO target. Returns the
    // txids and the total vsize actually offered.
    pub fn hybrid_round(
        &mut self,
        deficit_vsize: u64,
        small_txs: u64,
        fanout: (u64, u64),
        replaceable: bool,
        replaces: u64,
        checkpoint: &impl Fn(&str) -> bool,
    ) -> (Vec<Txid>, u64) {
        if !self.ensure_funds(fanout.0, fanout.1, checkpoint) {
            return (Vec::new(), 0);
        }

        let mut txids: Vec<Txid> = Vec::new();
        let mut sent: Vec<SentSpam> = Vec::new();
        let mut added: u64 = 0;
        let mut first_error: Option<String> = None;
        let mut sealer_count = 0u64;
        let mut data_count = 0u64;

        // Small payment-shaped txs first: cosmetic minimum-size traffic in the
        // mempool. (The airtight packing guarantee is the floor_round pool.)
        let mut fails = 0;
        while sealer_count < small_txs {
            if self.utxos.is_empty()
                || fails >= self.utxos.len()
                || !checkpoint("raw_sealer_before_submit")
            {
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
                        first_error = Some(e.to_string());
                    }
                    fails += 1;
                }
            }
            if !checkpoint("raw_sealer_after_submit") {
                break;
            }
        }

        // Bulk fill with varied-size data txs up to the requested weight.
        fails = 0;
        while added < deficit_vsize {
            if self.utxos.is_empty()
                || fails >= self.utxos.len()
                || !checkpoint("raw_data_before_submit")
            {
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
                        first_error = Some(e.to_string());
                    }
                    fails += 1;
                }
            }
            if !checkpoint("raw_data_after_submit") {
                break;
            }
        }

        tracing::info!(
            "{} => Hybrid: {sealer_count} gap-sealers + {data_count} data txs, ~{}k vB offered",
            self.label,
            added / 1000
        );
        if added < deficit_vsize {
            let detail = first_error.map(|e| format!(", first error: {e}")).unwrap_or_else(|| {
                ", branch pool exhausted (raise SPAM_FANOUT_UTXOS / SPAM_FILL_BLOCK_RATIO headroom)"
                    .to_string()
            });
            tracing::warn!(
                "{} only offered ~{}k/{}k vB this block{detail}",
                self.label,
                added / 1000,
                deficit_vsize / 1000
            );
        }
        if replaceable {
            self.bump_spam_txs(&sent, replaces, checkpoint);
        }
        (txids, added)
    }
}
