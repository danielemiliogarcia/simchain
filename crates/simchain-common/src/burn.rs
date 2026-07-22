//! Deterministic burn destinations shared by transaction-generating tools.

use bitcoincore_rpc::bitcoin::{
    hashes::{hash160, Hash},
    Address, Network, ScriptBuf, WPubkeyHash,
};

/// Return a deterministic P2WPKH burn address for `index`.
///
/// The witness program is the hash of a fixed public tag, not a public key
/// whose private key is known. Outputs sent here therefore stay outside the
/// miner wallets and do not make wallet coin selection progressively slower.
pub fn burn_address(index: u64) -> Address {
    let hash = hash160::Hash::hash(format!("simchain-spam-burn-{index}").as_bytes());
    let script = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_raw_hash(hash));
    Address::from_script(&script, Network::Regtest).unwrap()
}
