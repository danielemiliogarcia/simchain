//! Wallet actions used by a reorg: inject transactions that only the winning
//! node saw before it mines its replacement chain.

use bitcoincore_rpc::{bitcoin::Amount, Client, RpcApi};
use simchain_common::config::RpcUrl;
use simchain_common::{create_wallet_client, require_regtest_address};

#[derive(Debug)]
pub struct TransactionInjection {
    pub txids: Vec<bitcoincore_rpc::bitcoin::Txid>,
    pub shortfall: Option<String>,
}

impl TransactionInjection {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            txids: Vec::new(),
            shortfall: Some(message.into()),
        }
    }
}

/// Resolve the reorg node's wallet, returning `(name, wallet-scoped client)`.
/// Prefers `REORG_WALLET_NAME` (the wallet the controller created on the reorg
/// node); falls back to the first loaded wallet if that one is not loaded, and
/// logs once when it does. Returns `None` if no wallet is loaded or the client
/// cannot be built. Shared so tx injection and the double-spend planner always
/// agree on which wallet the reorg node is acting as.
pub fn resolve_wallet(
    node: &Client,
    rpc_url: &RpcUrl,
    preferred_wallet: &str,
) -> Option<(String, Client)> {
    let wallet_name = match node.list_wallets() {
        Ok(wallets) if wallets.iter().any(|wallet| wallet == preferred_wallet) => {
            preferred_wallet.to_string()
        }
        Ok(wallets) if !wallets.is_empty() => {
            tracing::warn!(
                "Wallet '{}' not loaded on the reorg node, using '{}' instead",
                preferred_wallet,
                wallets[0]
            );
            wallets[0].clone()
        }
        _ => {
            tracing::warn!("No wallet loaded on the reorg node");
            return None;
        }
    };
    match create_wallet_client(rpc_url, &wallet_name) {
        Ok(wallet) => Some((wallet_name, wallet)),
        Err(error) => {
            tracing::error!("Wallet client build failed ({error})");
            None
        }
    }
}

/// Send `count` fresh transactions from a wallet on the reorg node into its
/// own mempool, modelling a node that received transactions its peers have not
/// yet seen (clients broadcasting only to it). Returns the txids that were
/// actually created plus the first reason a requested count was not reached.
pub fn inject_transactions(
    node: &Client,
    count: u64,
    rpc_url: &RpcUrl,
    preferred_wallet: &str,
) -> TransactionInjection {
    let Some((wallet_name, wallet)) = resolve_wallet(node, rpc_url, preferred_wallet) else {
        let message = "no usable wallet is loaded on the reorg node";
        tracing::warn!("Cannot add new transactions ({message})");
        return TransactionInjection::failed(message);
    };
    let address = match wallet.get_new_address(None, None) {
        Ok(address) => match require_regtest_address(address) {
            Ok(address) => address,
            Err(error) => {
                let message = format!("wallet address is not usable: {error}");
                tracing::warn!("{message}; skipping tx injection");
                return TransactionInjection::failed(message);
            }
        },
        Err(error) => {
            let message = format!("could not get an address from wallet '{wallet_name}': {error}");
            tracing::warn!("{message}; skipping tx injection");
            return TransactionInjection::failed(message);
        }
    };
    let mut txids = Vec::new();
    let mut shortfall = None;
    for _ in 0..count {
        match wallet.send_to_address(
            &address,
            Amount::from_sat(1000),
            None,
            None,
            None,
            None,
            None,
            None,
        ) {
            Ok(txid) => {
                txids.push(txid);
            }
            Err(error) => {
                let sent = txids.len();
                let message =
                    format!("transaction injection stopped after {sent}/{count}: {error}");
                tracing::warn!("{message}");
                shortfall = Some(message);
                break;
            }
        }
    }
    if let Some(sample_txid) = txids.first() {
        let sent = txids.len();
        tracing::info!(
            "Added {sent} new transactions from wallet '{wallet_name}' (txs this node saw first) to mine into the winning chain; sample injected txid: {sample_txid}"
        );
    } else {
        let message = format!("wallet '{wallet_name}' has no spendable funds");
        tracing::warn!("Could not add new transactions ({message})");
        shortfall.get_or_insert(message);
    }
    TransactionInjection { txids, shortfall }
}
