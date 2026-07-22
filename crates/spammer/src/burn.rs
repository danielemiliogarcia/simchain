//! Burn destinations and miner-split constants used by the spammer.

pub use simchain_common::burn_address;

// Wallets the spam is split across (node2 and node3). Shared with
// live_tuning so the legacy per-miner alias converts identically everywhere;
// if a miner is ever added or removed, updating the shared constant keeps
// SPAM_TXS_PER_BLOCK meaning "total txs per block" for the user.
pub use simchain_common::live_tuning::MINER_COUNT;
