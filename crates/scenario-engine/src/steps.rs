use crate::{
    burst,
    docker::Docker,
    rpc::{self, RpcClients},
    schema::Step,
};
use anyhow::{anyhow, Context, Result};
use bitcoincore_rpc::RpcApi;
use simchain_common::require_regtest_address;
use std::{
    thread,
    time::{Duration, Instant},
};

pub fn execute(
    step: &Step,
    rpc_clients: &RpcClients,
    docker: &Docker,
    timeout: Duration,
) -> Result<()> {
    match step {
        Step::WaitHeight { height } => {
            let (start, finish) = rpc::wait_for_height(rpc_clients.node1(), *height, timeout)?;
            tracing::info!(
                start_height = start,
                final_height = finish,
                "Height reached"
            );
        }
        Step::Sleep { secs } => thread::sleep(Duration::from_secs(*secs)),
        Step::PauseMining => {
            docker.pause_mining()?;
            // `compose stop` exits 0 even when it matched nothing (e.g. the
            // stack runs under a different COMPOSE_PROJECT_NAME), so confirm
            // the controller is actually down instead of trusting the exit
            // code -- symmetric with the resume_mining check below.
            let start = Instant::now();
            while docker.container_running(crate::docker::MINING_CONTROLLER)? {
                if start.elapsed() >= timeout {
                    return Err(anyhow!(
                        "timed out waiting for the mining controller to stop; \
                         is the stack running under a different compose project name?"
                    ));
                }
                thread::sleep(Duration::from_millis(500));
            }
        }
        Step::ResumeMining => {
            docker.resume_mining()?;
            let start = Instant::now();
            while !docker.container_running(crate::docker::MINING_CONTROLLER)? {
                if start.elapsed() >= timeout {
                    return Err(anyhow!("timed out waiting for mining controller to run"));
                }
                thread::sleep(Duration::from_millis(500));
            }
        }
        Step::Mine { node, blocks } => {
            let wallet = rpc_clients.wallet(*node);
            let address = require_regtest_address(
                wallet
                    .get_new_address(None, None)
                    .context("failed to get fresh mining address")?,
            )?;
            rpc_clients
                .node(*node)
                .generate_to_address(*blocks, &address)
                .with_context(|| format!("failed to mine {blocks} blocks on {node}"))?;
        }
        Step::Reorg { depth, empty } => docker.reorg(*depth, *empty)?,
        Step::SpamBurst {
            node,
            txs,
            outputs_per_tx,
        } => burst::send(rpc_clients.wallet(*node), *txs, *outputs_per_tx)?,
        Step::Partition {
            node,
            main_blocks,
            isolated_blocks,
        } => docker.partition(&node.to_string(), *main_blocks, *isolated_blocks)?,
    }
    Ok(())
}
