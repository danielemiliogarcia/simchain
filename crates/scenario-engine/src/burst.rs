use anyhow::{Context, Result};
use bitcoincore_rpc::{bitcoin::Amount, Client, RpcApi};
use serde_json::json;
use simchain_common::burn_address;

const DUST_SATS: u64 = 546;

pub fn send(wallet: &Client, txs: u64, outputs_per_tx: u64) -> Result<()> {
    if outputs_per_tx == 0 {
        let address = burn_address(0);
        for number in 1..=txs {
            wallet
                .send_to_address(
                    &address,
                    Amount::from_sat(DUST_SATS),
                    None,
                    None,
                    None,
                    Some(false),
                    None,
                    None,
                )
                .with_context(|| format!("spam transaction {number}/{txs} failed"))?;
        }
        return Ok(());
    }

    let mut amounts = serde_json::Map::new();
    for index in 1..=outputs_per_tx {
        amounts.insert(burn_address(index).to_string(), json!(0.00000546));
    }
    let params = [
        json!(""),
        json!(amounts),
        json!(1),
        json!("scenario spam burst"),
        json!([]),
        json!(false),
    ];
    for number in 1..=txs {
        wallet
            .call::<String>("sendmany", &params)
            .with_context(|| format!("spam batch {number}/{txs} failed"))?;
    }
    Ok(())
}
