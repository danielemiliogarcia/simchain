//! Shared Bitcoin address helpers.

use bitcoincore_rpc::bitcoin::{address::NetworkUnchecked, Address, Network};
use bitcoincore_rpc::{Client, Error as RpcError, RpcApi};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MiningAddressError {
    #[error("Bitcoin RPC failed while resolving the mining address: {0}")]
    Rpc(#[from] RpcError),
    #[error("mining wallet address must be a regtest address: {0}")]
    Address(#[from] bitcoincore_rpc::bitcoin::address::ParseError),
}

/// Ensure an address is explicitly a regtest address before any binary uses
/// it against the simulated network.
pub fn require_regtest_address(
    address: Address<NetworkUnchecked>,
) -> Result<Address, bitcoincore_rpc::bitcoin::address::ParseError> {
    address.require_network(Network::Regtest)
}

pub fn mining_address_label(wallet_name: &str) -> String {
    format!("simchain-miner-{wallet_name}")
}

pub fn get_or_create_mining_address(
    wallet: &Client,
    wallet_name: &str,
) -> Result<Address, MiningAddressError> {
    let label = mining_address_label(wallet_name);
    let existing =
        crate::rpc_retry("get labeled mining wallet address", || match wallet
            .call::<serde_json::Map<String, serde_json::Value>>(
                "getaddressesbylabel",
                &[label.clone().into()],
            ) {
            Ok(addresses) => Ok(Some(addresses)),
            Err(error) if is_no_addresses_with_label(&error) => Ok(None),
            Err(error) => Err(error),
        });
    if let Some(addresses) = existing {
        if let Some(address) = addresses.keys().min() {
            return Ok(require_regtest_address(Address::from_str(address)?)?);
        }
    } else {
        tracing::debug!("No existing mining address for label '{label}'");
    }

    Ok(require_regtest_address(crate::rpc_retry(
        "get new labeled mining wallet address",
        || wallet.get_new_address(Some(&label), None),
    ))?)
}

fn is_no_addresses_with_label(error: &RpcError) -> bool {
    matches!(
        error,
        RpcError::JsonRpc(bitcoincore_rpc::jsonrpc::error::Error::Rpc(error)) if error.code == -11
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mining_address_label_matches_mempool_pool_seeder() {
        assert_eq!(mining_address_label("node2"), "simchain-miner-node2");
        assert_eq!(mining_address_label("node3"), "simchain-miner-node3");
    }
}
