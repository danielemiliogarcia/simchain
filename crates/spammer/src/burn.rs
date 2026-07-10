//! Burn destinations and miner-split constants used by the spammer.

use bitcoincore_rpc::bitcoin::{
    hashes::{hash160, Hash},
    Address, Network, ScriptBuf, WPubkeyHash,
};

// Wallets the spam is split across (node2 and node3). If a miner is ever
// added or removed, updating this constant keeps SPAM_TXS_PER_BLOCK meaning
// "total txs per block" for the user.
pub const MINER_COUNT: u64 = 2;

// Spam destinations are burn addresses (P2WPKH over the hash of a fixed tag,
// no known key), not wallet addresses. Dust paid to a wallet address lands in
// that wallet, and bitcoind's coin selection scans every UTXO on each send:
// the old cross-wallet spam grew each miner wallet by one UTXO per spam
// output (~18k per full block in batch mode) until the send cycle no longer
// fit inside the block interval. Burning the dust keeps the wallets lean --
// they only accumulate their own change -- at the cost of slowly draining
// them (~0.16 BTC per full block against a ~2550 BTC bootstrap balance).
pub fn burn_address(index: u64) -> Address {
    let hash = hash160::Hash::hash(format!("simchain-spam-burn-{index}").as_bytes());
    let script = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_raw_hash(hash));
    Address::from_script(&script, Network::Regtest).unwrap()
}
