use bitcoincore_rpc::{
    bitcoin::{address::NetworkUnchecked, Address, BlockHash, Network},
    jsonrpc, Client, RpcApi,
};
use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, thread, time::Duration};

// How many recent blocks to remember for reorg analysis. Reorgs deeper than
// this window are still detected, but the fork point is then reported as the
// bottom of the window (the same rule chainwatch.sh uses).
const REORG_WINDOW: u64 = 100;

// Height at which the bootstrap sequence (funding + coinbase maturity) ends.
const BOOTSTRAP_END: u64 = 204;

// A node assembling a full 4M WU block under spam load can take longer than
// the default 15s RPC timeout; the client then dies on a WouldBlock socket
// error while the node quietly finishes the call. Generous timeout instead:
// a healthy call is unaffected, and a node that needs this long is wedged
// enough that crashing (and restarting) is the right outcome.
const RPC_TIMEOUT_SECS: u64 = 300;

// SplitMix64 is small, seedable, and has a stable stream across builds. Using it
// directly keeps MINING_RNG_SEED reproducible without adding an RNG dependency
// whose standard generator may change between crate versions.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    // Uniform in [0, 1), using the top 53 bits (the f64 mantissa width).
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }

    fn next_exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.next_f64()).ln()
    }

    // Rejection sampling avoids the small bias introduced by `random % upper`.
    fn next_below(&mut self, upper: u64) -> u64 {
        debug_assert!(upper > 0);
        let threshold = upper.wrapping_neg() % upper;
        loop {
            let value = self.next_u64();
            if value >= threshold {
                return value % upper;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MinerWeights {
    node2: u64,
    node3: u64,
    total: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct IntervalBounds {
    min: Option<f64>,
    max: Option<f64>,
}

impl IntervalBounds {
    fn apply(self, sample: f64) -> f64 {
        let above_min = self.min.map_or(sample, |min| sample.max(min));
        self.max.map_or(above_min, |max| above_min.min(max))
    }

    fn description(self) -> String {
        match (self.min, self.max) {
            (None, None) => "unbounded".to_string(),
            (Some(min), None) => format!("[{min}s, unbounded)"),
            (None, Some(max)) => format!("[0s, {max}s]"),
            (Some(min), Some(max)) => format!("[{min}s, {max}s]"),
        }
    }
}

fn parse_mean_secs(value: &str) -> u64 {
    let seconds: u64 = value
        .parse()
        .expect("BLOCK_INTERVAL_MEAN_SECS must be a positive integer");
    assert!(
        seconds > 0,
        "BLOCK_INTERVAL_MEAN_SECS must be a positive integer"
    );
    seconds
}

fn parse_interval_mode(value: &str) -> bool {
    match value {
        "fixed" => false,
        "poisson" => true,
        _ => panic!("BLOCK_INTERVAL_MODE must be 'fixed' or 'poisson', got '{value}'"),
    }
}

fn parse_interval_bound(key: &str, value: &str) -> Option<f64> {
    if value.trim().is_empty() {
        return None;
    }

    let seconds: f64 = value
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{key} must be a non-negative finite number"));
    assert!(
        seconds.is_finite() && seconds >= 0.0,
        "{key} must be a non-negative finite number"
    );
    assert!(
        Duration::try_from_secs_f64(seconds).is_ok(),
        "{key} is too large to represent as a duration"
    );
    Some(seconds)
}

fn parse_interval_bounds(min: &str, max: &str) -> IntervalBounds {
    let min = parse_interval_bound("BLOCK_INTERVAL_MIN_SECS", min);
    let max = parse_interval_bound("BLOCK_INTERVAL_MAX_SECS", max);
    if let Some(max) = max {
        assert!(
            max > 0.0,
            "BLOCK_INTERVAL_MAX_SECS must be greater than zero"
        );
    }
    if let (Some(min), Some(max)) = (min, max) {
        assert!(
            min <= max,
            "BLOCK_INTERVAL_MIN_SECS must not exceed BLOCK_INTERVAL_MAX_SECS"
        );
    }
    IntervalBounds { min, max }
}

// A mean outside the clamp range pins nearly every interval to a boundary --
// almost certainly a leftover bound after the mean was changed, not intent.
// Poisson mode only: fixed mode ignores the bounds, and the full-block recipes
// legitimately combine a long fixed interval with the default bounds.
fn validate_poisson_mean(mean_secs: u64, bounds: IntervalBounds) {
    let mean = mean_secs as f64;
    if let Some(min) = bounds.min {
        assert!(
            mean >= min,
            "BLOCK_INTERVAL_MEAN_SECS ({mean_secs}) is below BLOCK_INTERVAL_MIN_SECS ({min}): nearly every interval would clamp to the minimum. Raise the mean, lower the bound, or use fixed mode"
        );
    }
    if let Some(max) = bounds.max {
        assert!(
            mean <= max,
            "BLOCK_INTERVAL_MEAN_SECS ({mean_secs}) exceeds BLOCK_INTERVAL_MAX_SECS ({max}): nearly every interval would clamp to the maximum. Lower the mean, raise the bound, or use fixed mode"
        );
    }
}

fn parse_miner_weights(value: &str) -> Option<MinerWeights> {
    if value.trim().is_empty() {
        return None;
    }

    let parts: Vec<&str> = value.split(',').collect();
    assert!(
        parts.len() == 2,
        "MINER_WEIGHTS must have exactly 2 entries (node2,node3), got {}",
        parts.len()
    );
    let parse = |part: &str| {
        part.trim()
            .parse::<u64>()
            .expect("MINER_WEIGHTS must be two non-negative integers, e.g. 70,30")
    };
    let node2 = parse(parts[0]);
    let node3 = parse(parts[1]);
    let total = node2
        .checked_add(node3)
        .expect("MINER_WEIGHTS entries must not overflow u64 when added");
    assert!(total > 0, "MINER_WEIGHTS must not be 0,0");

    Some(MinerWeights {
        node2,
        node3,
        total,
    })
}

fn parse_rng_seed(value: &str) -> Option<u64> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.trim().parse().expect("MINING_RNG_SEED must be a u64"))
    }
}

fn entropy_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_nanos() as u64;
    nanos ^ (u64::from(std::process::id()).rotate_left(32))
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn create_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> Client {
    let (user, pass) = (rpc_user.to_string(), Some(rpc_pass.to_string()));
    let transport = jsonrpc::simple_http::SimpleHttpTransport::builder()
        .url(rpc_url)
        .expect("invalid RPC url")
        .auth(user, pass)
        .timeout(Duration::from_secs(RPC_TIMEOUT_SECS))
        .build();
    Client::from_jsonrpc(jsonrpc::client::Client::with_transport(transport))
}

fn wait_for_rpc(client: &Client, name: &str) {
    loop {
        match client.get_block_count() {
            Ok(_) => return,
            Err(_) => {
                println!("Waiting for {name} RPC...");
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

// Poll until the non-mining node reports at least `height`, so the next round
// starts from a shared tip whether mining switches nodes or repeats a winner.
fn wait_for_height(client: &Client, height: u64) {
    loop {
        match client.get_block_count() {
            Ok(h) if h >= height => return,
            _ => thread::sleep(Duration::from_millis(100)),
        }
    }
}

// Create the wallet and return a wallet-scoped client plus a fresh address.
// A wallet-scoped URL keeps working even if the user loads extra wallets on
// the node later (the generic RPC path breaks with more than one wallet).
// Restart-safe: if the wallet already exists on disk it is loaded instead,
// and if it is already loaded it is used as-is.
fn setup_wallet(
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    node: &Client,
    wallet_name: &str,
) -> (Client, Address) {
    if let Err(create_err) = node.create_wallet(wallet_name, None, None, None, None) {
        match node.load_wallet(wallet_name) {
            Ok(_) => println!("Wallet '{wallet_name}' already exists, loaded it"),
            Err(load_err) if load_err.to_string().contains("already loaded") => {
                println!("Wallet '{wallet_name}' already loaded, reusing it");
            }
            Err(load_err) => {
                panic!("wallet '{wallet_name}': create failed ({create_err}), load failed ({load_err})")
            }
        }
    }
    let wallet = create_client(
        &format!("{rpc_url}/wallet/{wallet_name}"),
        rpc_user,
        rpc_pass,
    );
    let address = wallet.get_new_address(None, None).unwrap();
    let address = address.require_network(Network::Regtest).unwrap();
    (wallet, address)
}

// The controller's view of the recent chain: the hash it last observed at
// each height, plus the set of hashes it mined itself. Comparing the node's
// chain against `seen` exposes reorgs (and their fork point), and any block
// missing from `own` was mined by someone else -- the reorg simulator, a
// manual generate call, etc.
struct ChainView {
    seen: BTreeMap<u64, BlockHash>,
    own: HashSet<BlockHash>,
}

impl ChainView {
    fn new() -> Self {
        ChainView {
            seen: BTreeMap::new(),
            own: HashSet::new(),
        }
    }

    fn record(&mut self, height: u64, hash: BlockHash, mined_by_us: bool) {
        self.seen.insert(height, hash);
        if mined_by_us {
            self.own.insert(hash);
        }
        // Keep both collections bounded to the reorg window.
        let floor = height.saturating_sub(REORG_WINDOW);
        while let Some((&h, _)) = self.seen.first_key_value() {
            if h >= floor {
                break;
            }
            if let Some(old) = self.seen.remove(&h) {
                self.own.remove(&old);
            }
        }
    }

    // Walk down from `below` to the highest recorded height whose hash still
    // matches the node's chain: the fork point (last common block).
    fn find_fork(&self, node: &Client, below: u64) -> u64 {
        for (&h, hash) in self.seen.range(..=below).rev() {
            if node.get_block_hash(h).ok().as_ref() == Some(hash) {
                return h;
            }
        }
        // Nothing matches: the reorg is deeper than the window.
        self.seen
            .first_key_value()
            .map_or(0, |(&h, _)| h.saturating_sub(1))
    }
}

// Reconcile the node's chain with the controller's recorded view and return
// the node's current tip. A rewritten block triggers a REORG report with the
// fork point, the replaced range and the new tip; every walked block the
// controller did not mine itself is flagged EXTERNAL.
fn sync_view(view: &mut ChainView, node: &Client, last: u64) -> u64 {
    let tip = match node.get_block_count() {
        Ok(t) => t,
        Err(_) => return last,
    };
    let base = last.min(tip);

    // If the hash recorded at min(last, tip) still matches, the chain only
    // grew; otherwise history at or below that height was rewritten.
    let reorged = match view.seen.get(&base) {
        Some(hash) => node.get_block_hash(base).ok().as_ref() != Some(hash),
        None => false,
    };

    let from = if reorged {
        let fork = view.find_fork(node, base);
        println!(
            "REORG detected: blocks [{}..{}] replaced; forked at [{}], new tip [{}], mining continues on the new chain",
            fork + 1, last, fork, tip
        );
        // Forget the replaced blocks (and drop them from `own`: a replaced
        // block of ours is no longer on the chain). The walk below records
        // their replacements.
        let stale = view.seen.split_off(&(fork + 1));
        for hash in stale.values() {
            view.own.remove(hash);
        }
        fork + 1
    } else {
        last + 1
    };

    for h in from..=tip {
        // A block can vanish mid-walk if another reorg lands right now; stop
        // and let the next round re-sync.
        let Ok(hash) = node.get_block_hash(h) else {
            break;
        };
        let mined_by_us = view.own.contains(&hash);
        if !mined_by_us {
            println!("EXTERNAL block [{h}] {hash} (not mined by this controller)");
        }
        view.record(h, hash, mined_by_us);
    }
    tip
}

fn main() {
    // Every setting has a default matching docker-compose.yml, so the tool
    // also runs standalone with no environment at all.
    let user_address = env_or(
        "USER_ADDRESS",
        "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr",
    );
    let mean_secs = parse_mean_secs(&env_or("BLOCK_INTERVAL_MEAN_SECS", "15"));
    let poisson = parse_interval_mode(&env_or("BLOCK_INTERVAL_MODE", "poisson"));
    let interval_bounds = parse_interval_bounds(
        &env_or("BLOCK_INTERVAL_MIN_SECS", "10"),
        &env_or("BLOCK_INTERVAL_MAX_SECS", "20"),
    );
    if poisson {
        validate_poisson_mean(mean_secs, interval_bounds);
    }
    let miner_weights = parse_miner_weights(&env_or("MINER_WEIGHTS", ""));
    // Parse a supplied seed even when stochastic modes are disabled, so a typo
    // cannot lie dormant until a mode is enabled later.
    let configured_seed = parse_rng_seed(&env_or("MINING_RNG_SEED", ""));
    let stochastic = poisson || miner_weights.is_some();
    let seed = configured_seed.unwrap_or_else(entropy_seed);
    let mut rng = Rng(seed);

    let rpc_user = env_or("BTC_RPC_USER", "foo");
    let rpc_pass = env_or("BTC_RPC_PASS", "rpcpassword");
    let wallet2_name = env_or("NODE2_WALLET_NAME", "node2");
    let wallet3_name = env_or("NODE3_WALLET_NAME", "node3");

    let node2_url = env_or("NODE2_RPC_URL", "http://btc-simnet-node2:18443");
    let node3_url = env_or("NODE3_RPC_URL", "http://btc-simnet-node3:18443");
    let node2 = create_client(&node2_url, &rpc_user, &rpc_pass);
    let node3 = create_client(&node3_url, &rpc_user, &rpc_pass);

    let user_address: Address<NetworkUnchecked> =
        user_address.parse().expect("Invalid Bitcoin address");
    let user_address = user_address.require_network(Network::Regtest).unwrap();

    println!("Waiting for nodes to be ready");
    wait_for_rpc(&node2, "node2");
    wait_for_rpc(&node3, "node3");

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
    let (_wallet2, addr2) = setup_wallet(&node2_url, &rpc_user, &rpc_pass, &node2, &wallet2_name);
    let (_wallet3, addr3) = setup_wallet(&node3_url, &rpc_user, &rpc_pass, &node3, &wallet3_name);

    // Each stage ends at a fixed height, so the sequence is resumable: on
    // restart a completed stage is skipped (height already >= its target)
    // and an interrupted batch mines only its missing remainder -- the chain
    // never gets extra blocks and the user is never funded twice. Coinbase
    // pays the stage address no matter which node mines, so resuming
    // mid-batch cannot misassign funds.
    // (target height, miner, sync witness, reward address, label)
    let stages: [(u64, &Client, &Client, &Address, &str); 8] = [
        (1, &node2, &node3, &addr2, "node2 wallet block"),
        (2, &node3, &node2, &addr3, "node3 wallet block"),
        (3, &node2, &node3, &user_address, "user funding block 3"),
        (4, &node3, &node2, &user_address, "user funding block 4"),
        (54, &node2, &node3, &addr2, "node2 funding batch"),
        (104, &node3, &node2, &addr3, "node3 funding batch"),
        (154, &node2, &node3, &addr2, "node2 maturity batch"),
        (204, &node3, &node2, &addr3, "node3 maturity batch"),
    ];
    assert_eq!(
        stages[stages.len() - 1].0,
        BOOTSTRAP_END,
        "stage table must end at BOOTSTRAP_END"
    );

    let mut height = node2.get_block_count().unwrap();
    if height >= BOOTSTRAP_END {
        println!("Chain already bootstrapped (height {height}), skipping the funding sequence");
    } else if height > 0 {
        println!("Resuming interrupted bootstrap at height {height}");
    }
    for (target, miner, witness, addr, label) in stages {
        if height >= target {
            continue;
        }
        println!(
            "Bootstrap => Mining {} block(s) to address {addr} ({label}, up to height {target})",
            target - height
        );
        miner.generate_to_address(target - height, addr).unwrap();
        height = miner.get_block_count().unwrap();
        // Wait for the other node to sync before the next stage mines on
        // top, so blocks do not compete and stack on each other.
        wait_for_height(witness, height);
        println!("New block height: {height}");
    }

    println!(
        "\nActual block height: {}",
        node2.get_block_count().unwrap()
    );

    println!("\n//////////////////////////////////////////////////////////////////\n");
    println!("Funds in address {user_address} are mature and ready to spend.");
    println!("To list UTXOs, use scantxoutset or list_unspent from bdk crate");
    println!("\n//////////////////////////////////////////////////////////////////\n");

    // Continuous mining loop. The controller remembers the recent chain --
    // heights, hashes, and which blocks it mined itself -- so a reorg (the
    // reorg simulator rewriting recent blocks) is reported with its full
    // extent: fork point, replaced range and new tip, with the replacement
    // blocks flagged EXTERNAL because someone else mined them. Like a real
    // miner the controller keeps mining on whatever tip the node reports --
    // generate_to_address already does that -- so detection only makes the
    // events visible here; nothing needs to be controlled.
    let mut view = ChainView::new();
    let mut last = node2.get_block_count().unwrap();
    // Seed the view with the recent chain so even the first reorg gets an
    // accurate fork point. Bootstrap blocks are seeded as not-ours, which is
    // harmless: seeded heights are never re-walked unless a reorg replaces
    // them, and replacement blocks are external by definition.
    for h in last.saturating_sub(REORG_WINDOW)..=last {
        if let Ok(hash) = node2.get_block_hash(h) {
            view.record(h, hash, false);
        }
    }

    let bounds_description = interval_bounds.description();
    let interval_description = if poisson {
        format!("poisson mean={mean_secs}s, bounds={}", bounds_description)
    } else {
        format!("fixed {mean_secs}s")
    };
    let weights_description = match miner_weights {
        Some(weights) => format!("{},{} (node2,node3)", weights.node2, weights.node3),
        None => "alternate".to_string(),
    };
    if stochastic {
        println!(
            "Mining config: interval={interval_description}, weights={weights_description}, rng_seed={seed}"
        );
    } else {
        println!("Mining config: interval={interval_description}, weights={weights_description}");
    }

    let mut toggle = true;
    loop {
        let start_time = std::time::Instant::now();

        let target = if poisson {
            let sampled = rng.next_exp(mean_secs as f64);
            let target_secs = interval_bounds.apply(sampled);
            println!(
                "TIMING sampled interval {sampled:.2}s, target {target_secs:.2}s (poisson, mean {mean_secs}s, bounds={})",
                bounds_description
            );
            Duration::from_secs_f64(target_secs)
        } else {
            Duration::from_secs(mean_secs)
        };

        let pick_node2 = match miner_weights {
            Some(weights) => rng.next_below(weights.total) < weights.node2,
            None => toggle,
        };
        let (miner, other, addr, name) = if pick_node2 {
            (&node2, &node3, &addr2, "Node 2")
        } else {
            (&node3, &node2, &addr3, "Node 3")
        };

        // Catch up with the node before mining: report any reorg and any
        // externally mined blocks that appeared since the last round.
        last = sync_view(&mut view, miner, last);

        let mined = miner.generate_to_address(1, addr).unwrap();
        // Identify the new block by the hash generate returned instead of
        // the tip counter, which races with blocks arriving from elsewhere.
        let hash = mined[0];
        let mined_height = miner.get_block_header_info(&hash).unwrap().height as u64;
        println!("{name} => Mined 1 block [{mined_height}] {hash} to address {addr}");
        view.record(mined_height, hash, true);
        last = last.max(mined_height);
        wait_for_height(other, mined_height);

        toggle = !toggle;

        let elapsed = start_time.elapsed();
        if elapsed < target {
            thread::sleep(target - elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_stream_is_stable() {
        let mut rng = Rng(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn exponential_samples_have_expected_shape() {
        let mean = 5.0;
        let count = 100_000;
        let mut rng = Rng(42);
        let samples: Vec<f64> = (0..count).map(|_| rng.next_exp(mean)).collect();
        let actual_mean = samples.iter().sum::<f64>() / count as f64;
        let variance = samples
            .iter()
            .map(|sample| (sample - actual_mean).powi(2))
            .sum::<f64>()
            / count as f64;
        let coefficient_of_variation = variance.sqrt() / actual_mean;

        assert!((actual_mean - mean).abs() < 0.05);
        assert!((coefficient_of_variation - 1.0).abs() < 0.03);
        assert!(samples.iter().any(|sample| *sample < 1.0));
        assert!(samples.iter().any(|sample| *sample > 15.0));
    }

    #[test]
    fn weighted_draw_is_reproducible_and_proportional() {
        let mut first = Rng(42);
        let mut replay = Rng(42);
        let first_draws: Vec<u64> = (0..1_000).map(|_| first.next_below(100)).collect();
        let replay_draws: Vec<u64> = (0..1_000).map(|_| replay.next_below(100)).collect();
        assert_eq!(first_draws, replay_draws);

        let node2_wins = first_draws.iter().filter(|draw| **draw < 70).count();
        assert!((650..=750).contains(&node2_wins));
    }

    #[test]
    fn parses_supported_configuration() {
        assert_eq!(parse_mean_secs("15"), 15);
        assert!(!parse_interval_mode("fixed"));
        assert!(parse_interval_mode("poisson"));
        assert_eq!(
            parse_interval_bounds("", ""),
            IntervalBounds {
                min: None,
                max: None,
            }
        );
        assert_eq!(
            parse_interval_bounds("0.25", "30"),
            IntervalBounds {
                min: Some(0.25),
                max: Some(30.0),
            }
        );
        assert_eq!(parse_miner_weights(""), None);
        assert_eq!(parse_miner_weights("   "), None);
        assert_eq!(
            parse_miner_weights("70, 30"),
            Some(MinerWeights {
                node2: 70,
                node3: 30,
                total: 100,
            })
        );
        assert_eq!(parse_rng_seed("42"), Some(42));
        assert_eq!(parse_rng_seed(""), None);
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MEAN_SECS must be a positive integer")]
    fn rejects_zero_interval() {
        parse_mean_secs("0");
    }

    #[test]
    fn clamps_poisson_samples_to_configured_bounds() {
        let bounds = parse_interval_bounds("2.5", "10");
        assert_eq!(bounds.apply(0.1), 2.5);
        assert_eq!(bounds.apply(7.0), 7.0);
        assert_eq!(bounds.apply(20.0), 10.0);
    }

    #[test]
    fn accepts_poisson_mean_within_bounds() {
        validate_poisson_mean(15, parse_interval_bounds("10", "20"));
        validate_poisson_mean(15, parse_interval_bounds("", ""));
        validate_poisson_mean(10, parse_interval_bounds("10", "20"));
        validate_poisson_mean(20, parse_interval_bounds("10", "20"));
    }

    #[test]
    #[should_panic(expected = "exceeds BLOCK_INTERVAL_MAX_SECS")]
    fn rejects_poisson_mean_above_max() {
        validate_poisson_mean(60, parse_interval_bounds("10", "20"));
    }

    #[test]
    #[should_panic(expected = "is below BLOCK_INTERVAL_MIN_SECS")]
    fn rejects_poisson_mean_below_min() {
        validate_poisson_mean(5, parse_interval_bounds("10", "20"));
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MIN_SECS must not exceed")]
    fn rejects_reversed_interval_bounds() {
        parse_interval_bounds("10", "2");
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MAX_SECS must be greater than zero")]
    fn rejects_zero_max_interval() {
        parse_interval_bounds("", "0");
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MIN_SECS must be a non-negative finite number")]
    fn rejects_negative_min_interval() {
        parse_interval_bounds("-1", "");
    }

    #[test]
    #[should_panic(expected = "BLOCK_INTERVAL_MODE must be 'fixed' or 'poisson'")]
    fn rejects_unknown_interval_mode() {
        parse_interval_mode("gaussian");
    }

    #[test]
    #[should_panic(expected = "MINER_WEIGHTS must have exactly 2 entries")]
    fn rejects_extra_miner_weight() {
        parse_miner_weights("1,2,3");
    }

    #[test]
    #[should_panic(expected = "MINER_WEIGHTS must not be 0,0")]
    fn rejects_zero_miner_weights() {
        parse_miner_weights("0,0");
    }

    #[test]
    #[should_panic(expected = "MINER_WEIGHTS entries must not overflow u64")]
    fn rejects_miner_weight_overflow() {
        parse_miner_weights("18446744073709551615,1");
    }

    #[test]
    #[should_panic(expected = "MINING_RNG_SEED must be a u64")]
    fn rejects_invalid_seed() {
        parse_rng_seed("not-a-number");
    }
}
