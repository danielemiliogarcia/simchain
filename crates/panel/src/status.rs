//! Background status sampling: node1 RPC and container state land in one
//! shared snapshot so HTTP handlers never touch RPC or Docker themselves.

use crate::state::{
    SharedState, CONTROLLER_CONTAINER, NODE1_CONTAINER, NODE2_CONTAINER, NODE3_CONTAINER,
    SPAMMER_CONTAINER,
};
use bitcoincore_rpc::{Client, RpcApi};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const FAST_TICK: Duration = Duration::from_secs(2);
/// Slow work (recent blocks + `getrawmempool true`) runs every 3rd fast tick.
const SLOW_EVERY: u64 = 3;
/// 11 blocks give 10 timestamp deltas while displaying 10 blocks.
const CADENCE_BLOCKS: u64 = 11;

#[derive(Clone, Debug, Serialize)]
pub struct MempoolSummary {
    pub tx_count: usize,
    pub vbytes: usize,
    pub usage_bytes: usize,
    /// BTC/kvB, max(minrelaytxfee, mempool minimum).
    pub min_fee: f64,
    pub min_relay_fee: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BlockSummary {
    pub height: u64,
    pub hash: String,
    pub time: u64,
    /// Seconds since the previous block; None for the oldest fetched block.
    pub delta_secs: Option<i64>,
    pub tx_count: usize,
    pub size_bytes: usize,
    pub weight: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Cadence {
    pub mean_secs: f64,
    /// How many deltas the mean was computed from (10 when history allows).
    pub samples: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeeBucket {
    pub label: &'static str,
    pub count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServiceState {
    pub present: bool,
    pub status: String,
    pub running: bool,
    pub restarting: bool,
    pub exit_code: i64,
    pub restart_count: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StatusSnapshot {
    pub height: Option<u64>,
    pub best_hash: Option<String>,
    pub mempool: Option<MempoolSummary>,
    /// Newest first, at most 10 entries.
    pub recent_blocks: Vec<BlockSummary>,
    pub cadence: Option<Cadence>,
    pub fee_histogram: Vec<FeeBucket>,
    pub services: BTreeMap<String, ServiceState>,
    /// Epoch millis of the last successful fast sample; consumers treat an
    /// old value as "stale / RPC unavailable" instead of going blank.
    pub last_updated_ms: Option<u64>,
    /// Epoch millis of the last successful slow block/mempool sample.
    pub slow_last_updated_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docker_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slow_error: Option<String>,
    /// Aggregate retained for compatibility with existing clients.
    pub last_error: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn spawn_sampler(app: SharedState) {
    tokio::task::spawn_blocking(move || sampler_loop(app));
}

fn sampler_loop(app: SharedState) {
    let mut client: Option<Client> = None;
    let mut tick: u64 = 0;
    loop {
        let mut init_error = None;
        if client.is_none() {
            match simchain_common::create_client(&app.config.node1_url) {
                Ok(created) => client = Some(created),
                Err(error) => init_error = Some(format!("RPC client init failed: {error}")),
            }
        }
        // Docker sampling is independent and must continue even when the RPC
        // URL cannot currently be resolved or a client cannot be constructed.
        fast_sample(&app, client.as_ref(), init_error);
        if tick.is_multiple_of(SLOW_EVERY) {
            if let Some(client) = client.as_ref() {
                match slow_sample(&app, client) {
                    Ok(()) => {
                        let mut snapshot = app.status.write().expect("status lock");
                        snapshot.slow_error = None;
                        snapshot.slow_last_updated_ms = Some(now_ms());
                        refresh_last_error(&mut snapshot);
                    }
                    Err(error) => {
                        let mut snapshot = app.status.write().expect("status lock");
                        snapshot.slow_error = Some(error.to_string());
                        refresh_last_error(&mut snapshot);
                    }
                }
            }
        }
        tick += 1;
        std::thread::sleep(FAST_TICK);
    }
}

fn fast_sample(app: &SharedState, client: Option<&Client>, init_error: Option<String>) {
    let rpc = (|| -> anyhow::Result<(u64, String, MempoolSummary)> {
        let client = client.ok_or_else(|| {
            anyhow::anyhow!(
                "{}",
                init_error.unwrap_or_else(|| "RPC client unavailable".to_string())
            )
        })?;
        let height = client.get_block_count()?;
        let best_hash = client.get_best_block_hash()?;
        let info = client.get_mempool_info()?;
        Ok((
            height,
            best_hash.to_string(),
            MempoolSummary {
                tx_count: info.size,
                vbytes: info.bytes,
                usage_bytes: info.usage,
                min_fee: info.mempool_min_fee.to_btc(),
                min_relay_fee: info.min_relay_tx_fee.to_btc(),
            },
        ))
    })();

    let names = [
        CONTROLLER_CONTAINER,
        SPAMMER_CONTAINER,
        NODE1_CONTAINER,
        NODE2_CONTAINER,
        NODE3_CONTAINER,
    ];
    let docker = app.executor.inspect(&names).map(|inspected| {
        names
            .iter()
            .map(|name| {
                let state = match inspected.get(*name) {
                    Some(info) => ServiceState {
                        present: true,
                        status: info.status.clone(),
                        running: info.running,
                        restarting: info.restarting,
                        exit_code: info.exit_code,
                        restart_count: info.restart_count,
                    },
                    None => ServiceState {
                        present: false,
                        status: "absent".to_string(),
                        running: false,
                        restarting: false,
                        exit_code: 0,
                        restart_count: 0,
                    },
                };
                (name.to_string(), state)
            })
            .collect::<BTreeMap<_, _>>()
    });

    let mut snapshot = app.status.write().expect("status lock");
    match rpc {
        Ok((height, best_hash, mempool)) => {
            snapshot.height = Some(height);
            snapshot.best_hash = Some(best_hash);
            snapshot.mempool = Some(mempool);
            snapshot.last_updated_ms = Some(now_ms());
            snapshot.rpc_error = None;
        }
        Err(error) => snapshot.rpc_error = Some(error.to_string()),
    }
    match docker {
        Ok(services) => {
            snapshot.services = services;
            snapshot.docker_error = None;
        }
        Err(error) => snapshot.docker_error = Some(error.to_string()),
    }
    refresh_last_error(&mut snapshot);
}

fn refresh_last_error(snapshot: &mut StatusSnapshot) {
    let errors = [
        ("rpc", snapshot.rpc_error.as_deref()),
        ("docker", snapshot.docker_error.as_deref()),
        ("slow sample", snapshot.slow_error.as_deref()),
    ]
    .into_iter()
    .filter_map(|(label, error)| error.map(|error| format!("{label}: {error}")))
    .collect::<Vec<_>>();
    snapshot.last_error = (!errors.is_empty()).then(|| errors.join("; "));
}

fn slow_sample(app: &SharedState, client: &Client) -> anyhow::Result<()> {
    let height = client.get_block_count()?;
    let fetch = CADENCE_BLOCKS.min(height + 1);
    let mut blocks = Vec::with_capacity(fetch as usize);
    for offset in 0..fetch {
        let block_height = height - offset;
        let hash = client.get_block_hash(block_height)?;
        let info = client.get_block_info(&hash)?;
        blocks.push(BlockSummary {
            height: block_height,
            hash: hash.to_string(),
            time: info.time as u64,
            delta_secs: None,
            tx_count: info.n_tx,
            size_bytes: info.size,
            weight: info.weight,
        });
    }
    // blocks is newest-first; delta = this block's time minus the previous
    // (older) block's time. Timestamps are miner-supplied, so clamp at >= 0
    // display-side, not here.
    let mut deltas = Vec::new();
    for i in 0..blocks.len() {
        if i + 1 < blocks.len() {
            let delta = blocks[i].time as i64 - blocks[i + 1].time as i64;
            blocks[i].delta_secs = Some(delta);
            deltas.push(delta);
        }
    }
    let cadence = if deltas.is_empty() {
        None
    } else {
        Some(Cadence {
            mean_secs: deltas.iter().sum::<i64>() as f64 / deltas.len() as f64,
            samples: deltas.len(),
        })
    };
    // Show 10 blocks; the 11th exists only to give the 10th its delta.
    blocks.truncate(10);

    let histogram = fee_histogram(client)?;

    let mut snapshot = app.status.write().expect("status lock");
    snapshot.recent_blocks = blocks;
    snapshot.cadence = cadence;
    snapshot.fee_histogram = histogram;
    Ok(())
}

const BUCKETS: [(&str, f64, f64); 6] = [
    ("<5", 0.0, 5.0),
    ("5-10", 5.0, 10.0),
    ("10-20", 10.0, 20.0),
    ("20-50", 20.0, 50.0),
    ("50-100", 50.0, 100.0),
    ("100+", 100.0, f64::INFINITY),
];

fn fee_histogram(client: &Client) -> anyhow::Result<Vec<FeeBucket>> {
    let entries = client.get_raw_mempool_verbose()?;
    let mut counts = [0usize; BUCKETS.len()];
    for entry in entries.values() {
        if entry.vsize == 0 {
            continue;
        }
        let sat_vb = entry.fees.base.to_sat() as f64 / entry.vsize as f64;
        for (i, (_, low, high)) in BUCKETS.iter().enumerate() {
            if sat_vb >= *low && sat_vb < *high {
                counts[i] += 1;
                break;
            }
        }
    }
    Ok(BUCKETS
        .iter()
        .zip(counts)
        .map(|((label, _, _), count)| FeeBucket { label, count })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_error_keeps_independent_failures() {
        let mut snapshot = StatusSnapshot {
            docker_error: Some("daemon down".to_string()),
            slow_error: Some("verbose mempool failed".to_string()),
            ..StatusSnapshot::default()
        };
        refresh_last_error(&mut snapshot);
        let error = snapshot.last_error.as_ref().expect("aggregate error");
        assert!(error.contains("daemon down"));
        assert!(error.contains("verbose mempool failed"));

        snapshot.rpc_error = None;
        refresh_last_error(&mut snapshot);
        assert!(snapshot
            .last_error
            .expect("still present")
            .contains("daemon down"));
    }
}
