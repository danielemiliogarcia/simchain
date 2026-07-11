//! Burn destinations and miner-split constants used by the spammer.

pub use simchain_common::burn_address;

// Wallets the spam is split across (node2 and node3). If a miner is ever
// added or removed, updating this constant keeps SPAM_TXS_PER_BLOCK meaning
// "total txs per block" for the user.
pub const MINER_COUNT: u64 = 2;
