//! Background last-good sampling through Bitcoin RPC and private component
//! APIs. A failed probe marks only that component unreachable while retaining
//! its most recent useful telemetry.

use crate::state::{
    SharedState, MINING_COMPONENT, NODE1_COMPONENT, NODE2_COMPONENT, NODE3_COMPONENT,
    SPAM_COMPONENT,
};
use bitcoincore_rpc::{Client, RpcApi};
use serde_json::json;
pub use simchain_common::control_api::StatusResponse as StatusSnapshot;
use simchain_common::control_api::{
    BlockSummary, Cadence, ComponentState, ExplorerStatus, FeeBucket, ImpairmentSummary,
    MempoolSummary,
};
use simchain_common::internal_api::{MiningWorkerStatus, NetworkAgentStatus, SpamWorkerStatus};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FAST_TICK: Duration = Duration::from_secs(2);
const SLOW_EVERY: u64 = 3;
const CADENCE_BLOCKS: u64 = 11;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub fn spawn_sampler(app: SharedState) {
    tokio::task::spawn_blocking(move || sampler_loop(app));
}

fn sampler_loop(app: SharedState) {
    let mut node1: Option<Client> = None;
    let started = Instant::now();
    let mut tick = 0_u64;
    loop {
        let mut init_error = None;
        if node1.is_none() {
            match simchain_common::create_client(&app.config.node1_url) {
                Ok(client) => node1 = Some(client),
                Err(error) => init_error = Some(format!("RPC client init failed: {error}")),
            }
        }
        fast_sample(
            &app,
            node1.as_ref(),
            init_error,
            started.elapsed().as_secs(),
        );
        if tick.is_multiple_of(SLOW_EVERY) {
            slow_tick(&app, node1.as_ref());
        }
        tick += 1;
        std::thread::sleep(FAST_TICK);
    }
}

fn fast_sample(
    app: &SharedState,
    node1: Option<&Client>,
    init_error: Option<String>,
    uptime_secs: u64,
) {
    let primary = sample_primary(node1, init_error);
    let mut component_results = vec![
        (
            "control-plane".to_string(),
            Ok(ComponentState {
                reachable: true,
                status: "running".to_string(),
                uptime_secs: Some(uptime_secs),
                ..ComponentState::default()
            }),
        ),
        (
            MINING_COMPONENT.to_string(),
            app.mining
                .status()
                .map(mining_component)
                .map_err(|error| error.to_string()),
        ),
        (
            SPAM_COMPONENT.to_string(),
            app.spam
                .status()
                .map(spam_component)
                .map_err(|error| error.to_string()),
        ),
    ];
    component_results.push((
        NODE1_COMPONENT.to_string(),
        primary
            .as_ref()
            .map(|sample| node_component(sample.height))
            .map_err(Clone::clone),
    ));
    for (name, url) in [
        (NODE2_COMPONENT, &app.config.node2_url),
        (NODE3_COMPONENT, &app.config.node3_url),
    ] {
        component_results.push((
            name.to_string(),
            probe_node(url)
                .map(node_component)
                .map_err(|error| error.to_string()),
        ));
    }

    let mut network_results = Vec::new();
    for node in [NODE1_COMPONENT, NODE2_COMPONENT, NODE3_COMPONENT] {
        let result = app.network.status(node).map_err(|error| error.to_string());
        component_results.push((
            format!("network-agent-{node}"),
            result.clone().map(network_component),
        ));
        network_results.push((node, result));
    }

    let mut snapshot = app.status.write().expect("status lock");
    match primary {
        Ok(sample) => {
            snapshot.height = Some(sample.height);
            snapshot.best_hash = Some(sample.best_hash);
            snapshot.mempool = Some(sample.mempool);
            snapshot.last_updated_ms = Some(now_ms());
            snapshot.rpc_error = None;
        }
        Err(error) => snapshot.rpc_error = Some(error),
    }

    let mut component_errors = Vec::new();
    for (name, result) in component_results {
        update_component(
            &mut snapshot.components,
            &name,
            result,
            &mut component_errors,
        );
    }
    update_impairments(&mut snapshot.impairments, &network_results);
    snapshot.effective_generations = snapshot
        .components
        .iter()
        .filter(|(_, component)| component.reachable)
        .filter_map(|(name, component)| {
            component
                .effective_generation
                .map(|generation| (name.clone(), generation))
        })
        .collect();
    snapshot.component_error = (!component_errors.is_empty()).then(|| component_errors.join("; "));
    refresh_last_error(&mut snapshot);
}

#[derive(Debug)]
struct PrimarySample {
    height: u64,
    best_hash: String,
    mempool: MempoolSummary,
}

fn sample_primary(
    client: Option<&Client>,
    init_error: Option<String>,
) -> Result<PrimarySample, String> {
    (|| -> anyhow::Result<PrimarySample> {
        let client = client.ok_or_else(|| {
            anyhow::anyhow!(
                "{}",
                init_error.unwrap_or_else(|| "RPC client unavailable".to_string())
            )
        })?;
        let height = client.get_block_count()?;
        let best_hash = client.get_best_block_hash()?;
        let info = client.get_mempool_info()?;
        Ok(PrimarySample {
            height,
            best_hash: best_hash.to_string(),
            mempool: MempoolSummary {
                tx_count: info.size,
                vbytes: info.bytes,
                usage_bytes: info.usage,
                min_fee: info.mempool_min_fee.to_btc(),
                min_relay_fee: info.min_relay_tx_fee.to_btc(),
            },
        })
    })()
    .map_err(|error| error.to_string())
}

fn probe_node(url: &str) -> anyhow::Result<u64> {
    Ok(simchain_common::create_client(url)?.get_block_count()?)
}

fn node_component(height: u64) -> ComponentState {
    ComponentState {
        reachable: true,
        status: "reachable".to_string(),
        observed_height: Some(height),
        ..ComponentState::default()
    }
}

fn mining_component(status: MiningWorkerStatus) -> ComponentState {
    ComponentState {
        reachable: true,
        status: status.phase.as_str().to_string(),
        phase: Some(status.phase.as_str().to_string()),
        effective_generation: Some(status.effective_generation),
        uptime_secs: Some(status.uptime_secs),
        last_error: status.last_error,
        desired_state: Some(status.desired_state),
        effective_state: Some(status.effective_state),
        observed_height: status.height,
        next_scheduled_attempt_ms: status.next_scheduled_attempt_ms,
        last_mined_block: status.last_mined_block,
        active_lease_count: Some(status.active_leases.len()),
        ..ComponentState::default()
    }
}

fn spam_component(status: SpamWorkerStatus) -> ComponentState {
    ComponentState {
        reachable: true,
        status: status.phase.as_str().to_string(),
        phase: Some(status.phase.as_str().to_string()),
        effective_generation: Some(status.effective_generation),
        uptime_secs: Some(status.uptime_secs),
        last_error: status.last_error,
        desired_state: Some(status.desired_state),
        effective_state: Some(status.effective_state),
        observed_height: status.observed_height,
        active_lease_count: Some(status.active_leases.len()),
        cycle_phase: status.cycle_phase,
        accepted_transactions: Some(status.accepted_transactions),
        last_cycle_duration_ms: status.last_cycle_duration_ms,
        reconciliation_pending: Some(status.reconciliation_pending),
        spam_capacity: status.capacity,
        reconciliation_count: Some(status.reconciliation_count),
        last_reconciliation_reason: status.last_reconciliation_reason,
        ..ComponentState::default()
    }
}

fn network_component(status: NetworkAgentStatus) -> ComponentState {
    let impaired = status.active_lease.is_some();
    ComponentState {
        reachable: true,
        status: if impaired { "impaired" } else { "clear" }.to_string(),
        phase: Some(if impaired { "active" } else { "clear" }.to_string()),
        effective_generation: Some(status.effective_generation),
        uptime_secs: Some(status.uptime_secs),
        last_error: status.last_error,
        active_lease_count: Some(usize::from(impaired)),
        ..ComponentState::default()
    }
}

fn update_component(
    components: &mut BTreeMap<String, ComponentState>,
    name: &str,
    result: Result<ComponentState, String>,
    errors: &mut Vec<String>,
) {
    match result {
        Ok(component) => {
            components.insert(name.to_string(), component);
        }
        Err(error) => {
            errors.push(format!("{name}: {error}"));
            let component = components.entry(name.to_string()).or_default();
            component.reachable = false;
            component.status = "unreachable".to_string();
            component.last_error = Some(error);
        }
    }
}

fn update_impairments(
    impairments: &mut Vec<ImpairmentSummary>,
    results: &[(&str, Result<NetworkAgentStatus, String>)],
) {
    for (node, result) in results {
        let Ok(status) = result else {
            continue;
        };
        impairments.retain(|impairment| impairment.node != *node);
        if let Some(lease) = &status.active_lease {
            impairments.push(ImpairmentSummary {
                node: (*node).to_string(),
                kind: lease.impairment.kind().to_string(),
                owner_job_id: lease.owner_job_id.clone(),
            });
        }
    }
    impairments.sort_by(|left, right| left.node.cmp(&right.node));
}

fn refresh_last_error(snapshot: &mut StatusSnapshot) {
    let errors = [
        ("rpc", snapshot.rpc_error.as_deref()),
        ("components", snapshot.component_error.as_deref()),
        ("slow sample", snapshot.slow_error.as_deref()),
    ]
    .into_iter()
    .filter_map(|(label, error)| error.map(|error| format!("{label}: {error}")))
    .collect::<Vec<_>>();
    snapshot.last_error = (!errors.is_empty()).then(|| errors.join("; "));
}

fn slow_tick(app: &SharedState, client: Option<&Client>) {
    let slow_result = client
        .ok_or_else(|| anyhow::anyhow!("node1 RPC client unavailable"))
        .and_then(sample_chain_detail);
    let explorer = probe_explorer(&app.config.explorer_url, &app.config.explorer_probe_url);
    let mut snapshot = app.status.write().expect("status lock");
    match slow_result {
        Ok((blocks, cadence, histogram)) => {
            snapshot.recent_blocks = blocks;
            snapshot.cadence = cadence;
            snapshot.fee_histogram = histogram;
            snapshot.slow_last_updated_ms = Some(now_ms());
            snapshot.slow_error = None;
        }
        Err(error) => snapshot.slow_error = Some(error.to_string()),
    }
    snapshot.explorer = Some(explorer);
    refresh_last_error(&mut snapshot);
}

fn probe_explorer(public_url: &str, probe_url: &str) -> ExplorerStatus {
    match minreq::get(probe_url).with_timeout(2).send() {
        Ok(response) if (200..400).contains(&response.status_code) => ExplorerStatus {
            url: public_url.to_string(),
            reachable: true,
            error: None,
        },
        Ok(response) => ExplorerStatus {
            url: public_url.to_string(),
            reachable: false,
            error: Some(format!("probe returned HTTP {}", response.status_code)),
        },
        Err(error) => ExplorerStatus {
            url: public_url.to_string(),
            reachable: false,
            error: Some(error.to_string()),
        },
    }
}

fn sample_chain_detail(
    client: &Client,
) -> anyhow::Result<(Vec<BlockSummary>, Option<Cadence>, Vec<FeeBucket>)> {
    let height = client.get_block_count()?;
    let fetch = CADENCE_BLOCKS.min(height + 1);
    let mut blocks = Vec::with_capacity(fetch as usize);
    for offset in 0..fetch {
        let block_height = height - offset;
        let hash = client.get_block_hash(block_height)?;
        let info = client.get_block_info(&hash)?;
        let median_fee_rate_sat_vb = if offset == 0 {
            block_median_fee_rate_sat_vb(client, block_height)
                .ok()
                .flatten()
        } else {
            None
        };
        blocks.push(BlockSummary {
            height: block_height,
            hash: hash.to_string(),
            time: info.time as u64,
            delta_secs: None,
            tx_count: info.n_tx,
            size_bytes: info.size,
            weight: info.weight,
            median_fee_rate_sat_vb,
        });
    }
    let mut deltas = Vec::new();
    for index in 0..blocks.len() {
        if index + 1 < blocks.len() {
            let delta = blocks[index].time as i64 - blocks[index + 1].time as i64;
            blocks[index].delta_secs = Some(delta);
            deltas.push(delta);
        }
    }
    let cadence = (!deltas.is_empty()).then(|| Cadence {
        mean_secs: deltas.iter().sum::<i64>() as f64 / deltas.len() as f64,
        samples: deltas.len(),
    });
    blocks.truncate(10);
    Ok((blocks, cadence, fee_histogram(client)?))
}

fn block_median_fee_rate_sat_vb(client: &Client, height: u64) -> anyhow::Result<Option<f64>> {
    let stats = client.call::<serde_json::Value>(
        "getblockstats",
        &[json!(height), json!(["feerate_percentiles"])],
    )?;
    Ok(stats
        .get("feerate_percentiles")
        .and_then(|value| value.as_array())
        .and_then(|percentiles| percentiles.get(2))
        .and_then(|value| value.as_f64()))
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
    let mut counts = [0_usize; BUCKETS.len()];
    for entry in entries.values() {
        if entry.vsize == 0 {
            continue;
        }
        let sat_vb = entry.fees.base.to_sat() as f64 / entry.vsize as f64;
        for (index, (_, low, high)) in BUCKETS.iter().enumerate() {
            if sat_vb >= *low && sat_vb < *high {
                counts[index] += 1;
                break;
            }
        }
    }
    Ok(BUCKETS
        .iter()
        .zip(counts)
        .map(|((label, _, _), count)| FeeBucket {
            label: (*label).to_string(),
            count,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_component_probe_preserves_last_good_fields() {
        let mut components = BTreeMap::from([(
            "mining".to_string(),
            ComponentState {
                reachable: true,
                status: "running".to_string(),
                effective_generation: Some(4),
                observed_height: Some(100),
                ..ComponentState::default()
            },
        )]);
        let mut errors = Vec::new();
        update_component(
            &mut components,
            "mining",
            Err("connection refused".to_string()),
            &mut errors,
        );
        let mining = &components["mining"];
        assert!(!mining.reachable);
        assert_eq!(mining.status, "unreachable");
        assert_eq!(mining.effective_generation, Some(4));
        assert_eq!(mining.observed_height, Some(100));
        assert_eq!(errors, vec!["mining: connection refused"]);
    }

    #[test]
    fn aggregate_error_keeps_independent_failures() {
        let mut snapshot = StatusSnapshot {
            component_error: Some("worker down".to_string()),
            slow_error: Some("verbose mempool failed".to_string()),
            ..StatusSnapshot::default()
        };
        refresh_last_error(&mut snapshot);
        let error = snapshot.last_error.expect("aggregate error");
        assert!(error.contains("worker down"));
        assert!(error.contains("verbose mempool failed"));
    }
}
