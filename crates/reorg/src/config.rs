//! Environment and CLI configuration for the reorg simulator.

use anyhow::{anyhow, Context};
use bitcoincore_rpc::bitcoin::{address::NetworkUnchecked, Address};
use simchain_common::{env_or, require_regtest_address};
use std::env;

/// Everything the simulator reads from the environment and the command line.
/// Every setting has a default matching docker-compose.yml.
pub struct Config {
    pub rpc_user: String,
    pub rpc_pass: String,
    pub node_name: String,
    pub rpc_port: String,
    pub rpc_url: String,
    pub mode: String,
    pub every: u64,
    pub adds_new_txs: u64,
    pub depth: u64,
    pub empty_mode: bool,
    pub mine_address: Address,
    pub witness_name: String,
    pub wallet_name: String,
}

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        let rpc_user = env_or("BTC_RPC_USER", "foo");
        let rpc_pass = env_or("BTC_RPC_PASS", "rpcpassword");
        let node_name = env_or("REORG_NODE", "btc-simnet-node3");
        let rpc_port = env_or("REORG_NODE_RPC_PORT", "18443");
        let mode = env_or("REORG_MODE", "once");
        let every: u64 = env_or("AUTO_REORG_EVERY_BLOCKS", "20")
            .parse()
            .expect("AUTO_REORG_EVERY_BLOCKS must be a positive integer");
        // Brand-new txs the reorg node mines into the winning chain, modelling a
        // node that received transactions its peers have not yet seen. Seeded into
        // the mempool before mining (0 disables). Ignored in empty mode.
        let adds_new_txs: u64 = env_or("REORG_ADDS_NEW_TXS", "5")
            .parse()
            .expect("REORG_ADDS_NEW_TXS must be a non-negative integer");

        // CLI arguments, order-independent, forwarded through simulate-reorg.sh:
        //   <depth>          the first bare number, else REORG_DEPTH
        //   empty | --empty  mine empty replacement blocks (chaos reorg) rather
        //                    than re-mining the orphaned txs. Chosen per run, not a
        //                    persistent setting, so a real reorg and an empty one
        //                    can be issued against the same running chain.
        let cli_args: Vec<String> = env::args().skip(1).collect();
        let empty_mode = cli_args.iter().any(|a| a == "empty" || a == "--empty");
        let depth: u64 = cli_args
            .iter()
            .find_map(|a| a.parse::<u64>().ok())
            .unwrap_or_else(|| {
                env_or("REORG_DEPTH", "3")
                    .parse()
                    .expect("REORG_DEPTH must be a positive integer")
            });
        if depth < 1 {
            return Err(anyhow!("Reorg depth must be at least 1"));
        }

        let mine_address: Address<NetworkUnchecked> = env_or(
            "REORG_MINE_ADDRESS",
            "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr",
        )
        .parse()
        .context("Invalid REORG_MINE_ADDRESS")?;
        let mine_address = require_regtest_address(mine_address)
            .context("REORG_MINE_ADDRESS must be a regtest address")?;

        // Witness node: another node polled after the reorg to confirm the whole
        // network adopted the new chain (node1 never mines, ideal witness).
        // REORG_WITNESS_NODE=none disables the check.
        let witness_name = env_or("REORG_WITNESS_NODE", "btc-simnet-node1");
        let wallet_name = env_or("REORG_WALLET_NAME", "node3");

        let rpc_url = format!("http://{node_name}:{rpc_port}");
        Ok(Config {
            rpc_user,
            rpc_pass,
            node_name,
            rpc_port,
            rpc_url,
            mode,
            every,
            adds_new_txs,
            depth,
            empty_mode,
            mine_address,
            witness_name,
            wallet_name,
        })
    }
}
