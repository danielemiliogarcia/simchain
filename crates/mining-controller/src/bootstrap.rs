//! Bootstrap sequence: fund the miner wallets and the user address, then
//! bury the coinbases past maturity so the chain hands off fully liquid.

use anyhow::anyhow;
use bitcoincore_rpc::{bitcoin::Address, Client, RpcApi};
use simchain_common::{rpc_retry, wait_for_height, RPC_RETRY_ATTEMPTS};
use std::{thread, time::Duration};

// Height at which the bootstrap sequence (funding + coinbase maturity) ends.
pub const BOOTSTRAP_END: u64 = 204;

// Bootstrap plan: block 1 to node2's wallet, block 2 to node3's wallet,
// blocks 3 and 4 to the user address, then two 50-block funding batches
// (to node2 then node3), then two 50-block maturity batches. The chain
// ends at height BOOTSTRAP_END (204). Coinbase maturity is 100 blocks,
// and node3's funding batch is mined last (heights 55-104, maturing
// 155-204), so burying to height 204 leaves BOTH miner wallets fully
// liquid at handoff (~51 mature coinbases, ~2550 BTC each) instead of a
// single mature reward. The maturity batches also go to the miner
// wallets, so their coinbases keep maturing during the run (heights
// 205-304), sustaining long sessions.
pub fn run(
    node2: &Client,
    node3: &Client,
    addr2: &Address,
    addr3: &Address,
    user_address: &Address,
) -> anyhow::Result<()> {
    // Each stage ends at a fixed height, so the sequence is resumable: on
    // restart a completed stage is skipped (height already >= its target)
    // and an interrupted batch mines only its missing remainder -- the chain
    // never gets extra blocks and the user is never funded twice. Coinbase
    // pays the stage address no matter which node mines, so resuming
    // mid-batch cannot misassign funds.
    // (target height, miner, sync witness, reward address, label)
    let stages: [(u64, &Client, &Client, &Address, &str); 8] = [
        (1, node2, node3, addr2, "node2 wallet block"),
        (2, node3, node2, addr3, "node3 wallet block"),
        (3, node2, node3, user_address, "user funding block 3"),
        (4, node3, node2, user_address, "user funding block 4"),
        (54, node2, node3, addr2, "node2 funding batch"),
        (104, node3, node2, addr3, "node3 funding batch"),
        (154, node2, node3, addr2, "node2 maturity batch"),
        (204, node3, node2, addr3, "node3 maturity batch"),
    ];
    assert_eq!(
        stages[stages.len() - 1].0,
        BOOTSTRAP_END,
        "stage table must end at BOOTSTRAP_END"
    );

    let mut height = rpc_retry("get pre-bootstrap block count", || node2.get_block_count());
    if height >= BOOTSTRAP_END {
        tracing::info!(
            "Chain already bootstrapped (height {height}), skipping the funding sequence"
        );
    } else if height > 0 {
        tracing::info!("Resuming interrupted bootstrap at height {height}");
    }
    for (target, miner, witness, addr, label) in stages {
        if height >= target {
            continue;
        }
        let mut delay = Duration::from_millis(500);
        let mut last_error = None;
        for attempt in 1..=RPC_RETRY_ATTEMPTS {
            // Re-read the live height before every generate attempt. A
            // timed-out generate may still have mined some or all blocks, so
            // only the missing remainder is safe to issue again.
            height = rpc_retry("get bootstrap stage height", || miner.get_block_count());
            if height >= target {
                break;
            }
            tracing::info!(
                "Bootstrap => Mining {} block(s) to address {addr} ({label}, up to height {target})",
                target - height
            );
            match miner.generate_to_address(target - height, addr) {
                Ok(_) => {}
                Err(error) => {
                    last_error = Some(error.to_string());
                    if attempt < RPC_RETRY_ATTEMPTS {
                        tracing::warn!(
                            "Bootstrap generate failed ({error}), retry {attempt}/{RPC_RETRY_ATTEMPTS} in {delay:?} after re-reading height"
                        );
                        thread::sleep(delay);
                        delay = (delay * 2).min(Duration::from_secs(30));
                    }
                }
            }
        }
        // One final state read catches a fifth timed-out call that completed
        // on the node before deciding the stage is truly wedged.
        height = rpc_retry("get completed bootstrap stage height", || {
            miner.get_block_count()
        });
        if height < target {
            return Err(anyhow!(
                "Bootstrap stage '{label}' did not reach height {target} after {RPC_RETRY_ATTEMPTS} generate attempts; last error: {}",
                last_error.as_deref().unwrap_or("generate returned success without reaching the target")
            ));
        }
        // Wait for the other node to sync before the next stage mines on
        // top, so blocks do not compete and stack on each other.
        wait_for_height(witness, height, Duration::from_millis(100));
        tracing::info!("New block height: {height}");
    }

    tracing::info!(
        "\nActual block height: {}",
        rpc_retry("get post-bootstrap block count", || node2.get_block_count())
    );

    tracing::info!("\n//////////////////////////////////////////////////////////////////\n");
    tracing::info!("Funds in address {user_address} are mature and ready to spend.");
    tracing::info!("To list UTXOs, use scantxoutset or list_unspent from bdk crate");
    tracing::info!("\n//////////////////////////////////////////////////////////////////\n");

    Ok(())
}
