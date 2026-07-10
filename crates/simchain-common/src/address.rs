//! Shared Bitcoin address helpers.

use bitcoincore_rpc::bitcoin::{address::NetworkUnchecked, Address, Network};

/// Ensure an address is explicitly a regtest address before any binary uses
/// it against the simulated network.
pub fn require_regtest_address(
    address: Address<NetworkUnchecked>,
) -> Result<Address, bitcoincore_rpc::bitcoin::address::ParseError> {
    address.require_network(Network::Regtest)
}
