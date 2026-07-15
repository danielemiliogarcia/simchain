//! Wallet readiness helpers for the spammer.

use bitcoincore_rpc::{bitcoin::Amount, Client, RpcApi};
use std::{thread, time::Duration};

/// Wait until the wallet exists and has at least 1 BTC of trusted (confirmed,
/// mature) balance, while remaining interruptible at cooperative boundaries.
/// Returns false when the caller requested a safe stop.
pub fn wait_for_funds_until(wallet: &Client, name: &str, checkpoint: impl Fn() -> bool) -> bool {
    tracing::info!("Waiting for wallet '{name}' funds to mature...");
    let minimum = Amount::from_btc(1.0).unwrap();
    let mut iterations = 0u64;
    loop {
        if !checkpoint() {
            return false;
        }
        match wallet.get_balances() {
            Ok(balances) if balances.mine.trusted >= minimum => return true,
            Ok(balances) => {
                if iterations > 0 && iterations.is_multiple_of(60) {
                    tracing::info!(
                        "Still waiting for wallet '{name}': trusted balance {:.8} BTC < 1 BTC (coinbase maturity)",
                        balances.mine.trusted.to_btc()
                    );
                }
            }
            Err(error) => {
                if iterations > 0 && iterations.is_multiple_of(60) {
                    tracing::info!(
                        "Still waiting for wallet '{name}': not loaded yet (the mining controller creates it during bootstrap) — {error}"
                    );
                }
            }
        }
        iterations += 1;
        thread::sleep(Duration::from_millis(500));
    }
}
