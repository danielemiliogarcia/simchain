use crate::{config::Config, schema::MinerNode};
use anyhow::{anyhow, Context, Result};
use bitcoincore_rpc::{Client, RpcApi};
use simchain_common::{create_client, create_wallet_client};
use std::{
    thread,
    time::{Duration, Instant},
};

pub struct RpcClients {
    node1: Client,
    node2: Client,
    node3: Client,
    wallet2: Client,
    wallet3: Client,
}

impl RpcClients {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            node1: create_client(&config.node1_url)?,
            node2: create_client(&config.node2_url)?,
            node3: create_client(&config.node3_url)?,
            wallet2: create_wallet_client(&config.node2_url, &config.node2_wallet)?,
            wallet3: create_wallet_client(&config.node3_url, &config.node3_wallet)?,
        })
    }

    pub fn node1(&self) -> &Client {
        &self.node1
    }

    pub fn node(&self, node: MinerNode) -> &Client {
        match node {
            MinerNode::Node2 => &self.node2,
            MinerNode::Node3 => &self.node3,
        }
    }

    pub fn wallet(&self, node: MinerNode) -> &Client {
        match node {
            MinerNode::Node2 => &self.wallet2,
            MinerNode::Node3 => &self.wallet3,
        }
    }
}

pub fn wait_for_rpc(client: &Client, timeout: Duration) -> Result<u64> {
    let start = Instant::now();
    loop {
        match client.get_block_count() {
            Ok(height) => return Ok(height),
            Err(error) if start.elapsed() >= timeout => {
                return Err(anyhow!(error)).context("timed out waiting for node1 RPC")
            }
            Err(_) => thread::sleep(Duration::from_millis(500)),
        }
    }
}

pub fn wait_for_height(client: &Client, target: u64, timeout: Duration) -> Result<(u64, u64)> {
    let start = Instant::now();
    let initial = client
        .get_block_count()
        .context("failed to query node1 height")?;
    loop {
        match client.get_block_count() {
            Ok(height) if height >= target => return Ok((initial, height)),
            Ok(height) if start.elapsed() >= timeout => {
                return Err(anyhow!(
                    "timed out waiting for node1 height {target}; current height is {height}"
                ));
            }
            Err(error) if start.elapsed() >= timeout => {
                return Err(anyhow!(error)).context(format!(
                    "timed out waiting for node1 height {target}; node1 RPC is unavailable"
                ));
            }
            Ok(_) | Err(_) => thread::sleep(Duration::from_millis(500)),
        }
    }
}
