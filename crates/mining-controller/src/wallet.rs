//! Mining-controller wallet bootstrap.

use anyhow::{anyhow, Context};
use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use simchain_common::{
    create_wallet_client, get_or_create_mining_address, mining_address_label, RpcUrl,
};

/// Create the mining wallet and return a wallet-scoped client plus a fresh
/// address. A wallet-scoped URL keeps working even if users load extra wallets
/// on the node. Restart-safe: an existing wallet is loaded or reused.
pub fn setup_wallet(
    rpc_url: &RpcUrl,
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
    let wallet =
        create_wallet_client(rpc_url, wallet_name).context("build wallet-scoped mining client")?;
    let label = mining_address_label(wallet_name);
    let address = get_or_create_mining_address(&wallet, wallet_name)
        .context("get stable mining wallet address")?;
    tracing::info!("Wallet '{wallet_name}' mining address label '{label}' => {address}");
    Ok((wallet, address))
}
