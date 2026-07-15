//! Domain RPC probes used by runtime configuration validation.

use crate::backend::ChainBackend;
use bitcoincore_rpc::RpcApi;
use std::time::Duration;

pub struct RpcChainBackend {
    node_urls: [String; 3],
}

impl RpcChainBackend {
    pub fn new(node1_url: String, node2_url: String, node3_url: String) -> Self {
        Self {
            node_urls: [node1_url, node2_url, node3_url],
        }
    }
}

impl ChainBackend for RpcChainBackend {
    fn node1_height(&self) -> anyhow::Result<u64> {
        Ok(simchain_common::create_client(&self.node_urls[0])?.get_block_count()?)
    }

    fn spam_min_fee(&self) -> anyhow::Result<f64> {
        let mut required = 0.0_f64;
        for url in &self.node_urls {
            let info = simchain_common::create_client(url)?.get_mempool_info()?;
            required = required
                .max(info.min_relay_tx_fee.to_btc())
                .max(info.mempool_min_fee.to_btc());
        }
        Ok(required)
    }

    fn wait(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
