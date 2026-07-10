//! Mining-controller wallet bootstrap.

use anyhow::{anyhow, Context};
use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use simchain_common::{create_wallet_client, require_regtest_address, rpc_retry};

/// Create the mining wallet and return a wallet-scoped client plus a fresh
/// address. A wallet-scoped URL keeps working even if users load extra wallets
/// on the node. Restart-safe: an existing wallet is loaded or reused.
pub fn setup_wallet(
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    node: &Client,
    wallet_name: &str,
) -> anyhow::Result<(Client, Address)> {
    if let Err(create_err) = node.create_wallet(wallet_name, None, None, None, None) {
        match node.load_wallet(wallet_name) {
            Ok(_) => tracing::info!("Wallet '{wallet_name}' already exists, loaded it"),
            Err(load_err) if load_err.to_string().contains("already loaded") => {
                tracing::info!("Wallet '{wallet_name}' already loaded, reusing it");
            }
            Err(load_err) => {
                return Err(anyhow!(
                    "wallet '{wallet_name}': create failed ({create_err}), load failed ({load_err})"
                ));
            }
        }
    }
    let wallet = create_wallet_client(rpc_url, wallet_name, rpc_user, rpc_pass)
        .context("build wallet-scoped mining client")?;
    let address = rpc_retry("get new mining wallet address", || {
        wallet.get_new_address(None, None)
    });
    let address = require_regtest_address(address)
        .context("mining wallet address must be a regtest address")?;
    Ok((wallet, address))
}
