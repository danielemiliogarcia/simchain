//! Wallet actions used by a reorg: inject transactions that only the winning
//! node saw before it mines its replacement chain.

use crate::config::ReorgConfig;
use bitcoincore_rpc::{bitcoin::Amount, Client, RpcApi};
use simchain_common::{create_wallet_client, require_regtest_address};

/// Send `count` fresh transactions from a wallet on the reorg node into its
/// own mempool, modelling a node that received transactions its peers have not
/// yet seen (clients broadcasting only to it). Prefers `REORG_WALLET_NAME`
/// (the wallet the controller created on the reorg node); falls back to the
/// first loaded wallet if that one is not loaded.
pub fn inject_transactions(node: &Client, count: u64) {
    let config = ReorgConfig::global();
    let wallet_name = match node.list_wallets() {
        Ok(wallets) if wallets.contains(&config.wallet_name) => config.wallet_name.clone(),
        Ok(wallets) if !wallets.is_empty() => {
            tracing::warn!(
                "Wallet '{}' not loaded on the reorg node, using '{}' instead",
                config.wallet_name,
                wallets[0]
            );
            wallets[0].clone()
        }
        _ => {
            tracing::warn!("No wallet loaded on the reorg node, cannot add new transactions");
            return;
        }
    };
    let wallet = match create_wallet_client(&config.rpc_url, &wallet_name) {
        Ok(wallet) => wallet,
        Err(error) => {
            tracing::error!("Wallet client build failed ({error}), skipping tx injection");
            return;
        }
    };
    let address = match wallet.get_new_address(None, None) {
        Ok(address) => match require_regtest_address(address) {
            Ok(address) => address,
            Err(error) => {
                tracing::warn!("Wallet address not usable ({error}), skipping tx injection");
                return;
            }
        },
        Err(error) => {
            tracing::warn!(
                "Could not get an address from wallet '{wallet_name}' ({error}), skipping tx injection"
            );
            return;
        }
    };
    let mut sent = 0;
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
            Ok(_) => sent += 1,
            Err(error) => {
                tracing::warn!("Tx injection stopped after {sent} txs: {error}");
                break;
            }
        }
    }
    if sent > 0 {
        tracing::info!(
            "Added {sent} new transactions from wallet '{wallet_name}' (txs this node saw first) to mine into the winning chain"
        );
    } else {
        tracing::warn!(
            "Could not add new transactions (wallet '{wallet_name}' has no spendable funds)"
        );
    }
}
