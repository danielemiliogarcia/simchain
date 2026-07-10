//! Errors specific to locally-built raw spam transactions.

/// Failures a single spam or floor-fill send can encounter. `Rpc` carries the
/// node message because the raw engine inspects it to decide whether a branch
/// is stale (missing, conflicted, or spent) and must be discarded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SpamError {
    #[error("no usable branch")]
    NoUsableBranch,
    #[error("branch too small for this tx")]
    BranchTooSmall,
    #[error("missing batch response")]
    MissingBatchResponse,
    #[error("bitcoind returned an invalid txid")]
    InvalidTxid,
    #[error("{0}")]
    Rpc(String),
}
