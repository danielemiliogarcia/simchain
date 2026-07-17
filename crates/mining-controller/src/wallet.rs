//! Mining-controller wallet bootstrap.

use anyhow::{anyhow, Context};
use bitcoincore_rpc::{bitcoin::Address, Client, Error as RpcError, RpcApi};
use simchain_common::{create_wallet_client, require_regtest_address, rpc_retry, RpcUrl};
use std::str::FromStr;

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
    let address = get_or_create_mining_address(&wallet, &label)
        .context("get stable mining wallet address")?;
    tracing::info!("Wallet '{wallet_name}' mining address label '{label}' => {address}");
    Ok((wallet, address))
}

fn mining_address_label(wallet_name: &str) -> String {
    format!("simchain-miner-{wallet_name}")
}

fn get_or_create_mining_address(wallet: &Client, label: &str) -> anyhow::Result<Address> {
    let existing = rpc_retry("get labeled mining wallet address", || match wallet
        .call::<serde_json::Map<String, serde_json::Value>>("getaddressesbylabel", &[label.into()])
    {
        Ok(addresses) => Ok(Some(addresses)),
        Err(error) if is_no_addresses_with_label(&error) => Ok(None),
        Err(error) => Err(error),
    });
    match existing {
        Some(addresses) => {
            if let Some(address) = addresses.keys().min() {
                return require_regtest_address(Address::from_str(address)?)
                    .context("stored mining wallet address must be a regtest address");
            }
        }
        None => {
            tracing::debug!("No existing mining address for label '{label}'");
        }
    }

    let address = rpc_retry("get new labeled mining wallet address", || {
        wallet.get_new_address(Some(label), None)
    });
    require_regtest_address(address).context("mining wallet address must be a regtest address")
}

fn is_no_addresses_with_label(error: &RpcError) -> bool {
    matches!(
        error,
        RpcError::JsonRpc(bitcoincore_rpc::jsonrpc::error::Error::Rpc(error)) if error.code == -11
    )
}
