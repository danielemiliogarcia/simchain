# Implementation plan: Poisson block timing + weighted miner selection

**Status: implemented and amended 2026-07-10.** The shipped implementation follows this
plan with correctness hardenings for zero intervals and overflowing weight totals, and
uses rejection sampling instead of biased modulo reduction. Bounded Poisson timing is
now the default; its clamp behavior is intended and is not a tail-distribution bug.

Implements realistic block timing and hashrate distribution as independently configurable
timing and miner-selection modes. The default is bounded exponential timing with a
15-second underlying mean clamped to 10–20 seconds, plus strict node2/node3 alternation.
The controller logs the resolved mining configuration once at startup.

All work is contained in `mining-controller/src/main.rs` plus wiring
(`docker-compose.yml`, `.env.example`, `.env.full.example`) and docs
(`docs/SETTINGS.md`, `README.md`). No bitcoind flags are touched — this is pure
controller-side scheduling, so there is no relay/mempool policy-drift concern.

## Design decisions (locked — discussed and approved 2026-07-10)

1. **`BLOCK_INTERVAL_MODE=fixed|poisson`**, default `poisson`.
   `BLOCK_INTERVAL_MEAN_SECS` is the exact interval in fixed mode and the underlying
   exponential mean in Poisson mode. It replaces `BLOCK_INTERVAL_SECS`; the old name is
   intentionally not retained as an alias so configuration has one unambiguous source.
2. **`MINER_WEIGHTS` unset/empty → strict alternation** (exactly today's toggle).
   Set (e.g. `70,30`) → weighted random pick per block, positionally node2,node3.
   Note `50,50` is a fair coin flip, NOT alternation — same miner can win several
   blocks in a row, and that streakiness is intentional (realistic race behavior).
3. **The two features are orthogonal.** Poisson timing with alternation, weighted
   miners with fixed timing, both, or neither — all four combinations valid.
4. **Optional explicit tail clamping.** Poisson mode first samples the pure exponential,
   then clamps it to `BLOCK_INTERVAL_MIN_SECS` and/or `BLOCK_INTERVAL_MAX_SECS` when set.
   The default bounds are 10 and 20 seconds; explicitly empty bounds restore unbounded
   behavior. Clamping, rather than rejection/resampling, intentionally creates repeated
   values (probability mass) at
   the bounds and changes the observed mean. `BLOCK_INTERVAL_MEAN_SECS` always names the
   pre-clamp exponential mean. The timing log records both raw sample and final target
   so this behavior is visible and must not be interpreted as a sampler defect. With a
   bound configured, arrivals form a bounded renewal process rather than a mathematically
   pure Poisson process; `poisson` refers to the underlying exponential sampler. Empty
   bounds retain exact Poisson-process behavior. In Poisson mode the mean must lie within
   the configured bounds: a mean outside the clamp range would pin nearly every interval
   to a boundary, which is almost always a leftover bound after changing the mean, so
   startup fails instead. Fixed mode skips this check — it ignores the bounds, and the
   full-block recipes legitimately pair a long fixed interval with the default bounds.
5. **`MINING_RNG_SEED`** — optional u64. Same seed → identical interval sequence and
   miner picks. Unset → seeded from system time nanos. The resolved seed is always
   logged at startup when any stochastic mode is active, so any run can be replayed
   after the fact.
6. **No new crate dependencies.** Hand-rolled SplitMix64 PRNG (~10 lines) instead of
   `rand`/`rand_distr`. Reasons: the repo deliberately keeps crates RNG-free (see
   `fee-market-plan.md` §1 — the spammer uses a deterministic multiplicative hash for
   the same reason, and `mining-controller/Cargo.toml` has exactly one dependency);
   and `rand::StdRng` explicitly does NOT guarantee stable streams across crate
   versions, which would silently break seed reproducibility on a dependency bump.
   SplitMix64 is public-domain, 3 lines of arithmetic, and stable forever.
7. **Fail fast on bad config.** Unknown `BLOCK_INTERVAL_MODE`, malformed or reversed
   interval bounds, non-positive mean, a Poisson mean outside the configured bounds,
   malformed `MINER_WEIGHTS` (not exactly 2 comma-separated non-negative integers, both
   zero, or overflowing total), or malformed `MINING_RNG_SEED` causes a startup panic
   with a clear message.
8. **Bootstrap untouched.** The staged funding sequence (heights 1–204) keeps its
   deterministic miner assignment; the new modes apply only to the continuous mining
   loop after bootstrap.

## New settings

| Variable | Default | Meaning |
|---|---|---|
| `BLOCK_INTERVAL_MEAN_SECS` | `15` | Exact interval in fixed mode; pre-clamp exponential mean in Poisson mode. Replaces `BLOCK_INTERVAL_SECS`. |
| `BLOCK_INTERVAL_MODE` | `poisson` | `poisson`: sample an exponential interval using the configured mean, then apply bounds. `fixed`: always target `BLOCK_INTERVAL_MEAN_SECS`. |
| `BLOCK_INTERVAL_MIN_SECS` | `10` | Poisson lower clamp; fractional seconds accepted. Set empty for zero. Validated but does not affect fixed mode. |
| `BLOCK_INTERVAL_MAX_SECS` | `20` | Poisson upper clamp; fractional seconds accepted. Set empty for unbounded. Validated but does not affect fixed mode. |
| `MINER_WEIGHTS` | *(empty)* | Empty: strict node2/node3 alternation (today's behavior). `W2,W3` (non-negative integers, e.g. `70,30`): each block's miner is drawn randomly with those relative weights. `0,100` and `100,0` are valid (single-miner). |
| `MINING_RNG_SEED` | *(empty)* | Empty: seed from entropy (system-time nanos). Set to any u64: reproducible interval + miner-pick sequence. Ignored (but harmless) when both stochastic modes are off. |

For migration, rename `BLOCK_INTERVAL_SECS` to `BLOCK_INTERVAL_MEAN_SECS`. Fixed mode
still lands on that exact cadence. In bounded Poisson mode, do not expect the arithmetic
mean of observed targets to equal it because clamping changes the output distribution.

## Implementation steps

### 1. PRNG + exponential sampling (`mining-controller/src/main.rs`)

Add a small self-contained PRNG near the top of the file:

```rust
// SplitMix64: tiny, seedable, stable-across-versions PRNG. Deliberately not the
// `rand` crate: StdRng streams may change between rand versions, which would break
// MINING_RNG_SEED reproducibility on a dependency bump.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    // Uniform in [0, 1): top 53 bits, the full mantissa precision of f64.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    // Exponential with the given mean, via inverse-transform sampling.
    // 1.0 - u is in (0, 1], so ln() never sees zero.
    fn next_exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.next_f64()).ln()
    }
}
```

Seed resolution in `main()`:

```rust
let seed: u64 = match env::var("MINING_RNG_SEED") {
    Ok(s) if !s.is_empty() => s.parse().expect("MINING_RNG_SEED must be a u64"),
    _ => std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64
        ^ std::process::id() as u64,
};
let mut rng = Rng(seed);
```

### 2. Config parsing in `main()`

```rust
let interval_mode = env_or("BLOCK_INTERVAL_MODE", "poisson");
if interval_mode != "fixed" && interval_mode != "poisson" {
    panic!("BLOCK_INTERVAL_MODE must be 'fixed' or 'poisson', got '{interval_mode}'");
}
let poisson = interval_mode == "poisson";

let mean_secs = parse_mean_secs(&env_or("BLOCK_INTERVAL_MEAN_SECS", "15"));
let interval_bounds = parse_interval_bounds(
    &env_or("BLOCK_INTERVAL_MIN_SECS", "10"),
    &env_or("BLOCK_INTERVAL_MAX_SECS", "20"),
);

// Empty/unset => None => strict alternation (exactly the current behavior).
let miner_weights: Option<(u64, u64)> = match env_or("MINER_WEIGHTS", "").as_str() {
    "" => None,
    s => {
        let parts: Vec<u64> = s
            .split(',')
            .map(|p| p.trim().parse().expect("MINER_WEIGHTS must be two non-negative integers, e.g. 70,30"))
            .collect();
        assert!(parts.len() == 2, "MINER_WEIGHTS must have exactly 2 entries (node2,node3), got {}", parts.len());
        assert!(parts[0] + parts[1] > 0, "MINER_WEIGHTS must not be 0,0");
        Some((parts[0], parts[1]))
    }
};
```

Log the effective configuration once, right before entering the continuous loop
(after the bootstrap prints), so every run records how it was configured:

```text
Mining config: interval=poisson mean=15s, bounds=[10s, 20s], weights=70,30 (node2,node3), rng_seed=1234567890
Mining config: interval=fixed 15s, weights=alternate
```

Print `rng_seed=` whenever `poisson || miner_weights.is_some()`, even if it came from
entropy — that line is what makes a crashed run replayable.

### 3. Continuous loop changes (`main.rs:312-342` as of commit 2e13c2e)

Replace the `toggle` logic minimally — keep the loop structure, the reorg `sync_view`
call, the `wait_for_height` sync, and the elapsed-time subtraction exactly as they are:

```rust
let mut toggle = true;
loop {
    let start_time = std::time::Instant::now();

    // Target interval: exact mean in fixed mode; bounded sample in Poisson mode.
    let target = if poisson {
        let sampled = rng.next_exp(mean_secs as f64);
        let target_secs = interval_bounds.apply(sampled);
        println!("TIMING sampled interval {sampled:.2}s, target {target_secs:.2}s ...");
        Duration::from_secs_f64(target_secs)
    } else {
        Duration::from_secs(mean_secs)
    };

    // Miner for this round: weighted draw, or the existing strict toggle.
    let pick_node2 = match miner_weights {
        Some((w2, w3)) => rng.next_below(w2 + w3) < w2,
        None => toggle,
    };
    let (miner, other, addr, name) = if pick_node2 {
        (&node2, &node3, &addr2, "Node 2")
    } else {
        (&node3, &node2, &addr3, "Node 3")
    };

    // ... existing body unchanged: sync_view, generate_to_address, record,
    //     wait_for_height(other, mined_height) ...

    toggle = !toggle; // harmless in weighted mode; keeps alternation state otherwise

    let elapsed = start_time.elapsed();
    if elapsed < target {
        thread::sleep(target - elapsed);
    }
}
```

Notes for the implementer:

- **Keep subtracting elapsed mining time.** The effective target (the raw sample after
  optional bounds) models the full block-to-block inter-arrival, not the sleep after
  mining finishes.
  If `elapsed >= target` (short sample under spam load), skip the sleep entirely —
  the existing `if elapsed < ...` guard already does this.
- **Bounds clamp; they do not truncate by resampling.** A raw sample below the minimum
  becomes exactly the minimum and one above the maximum becomes exactly the maximum.
  Repeated boundary values and a shifted observed mean are expected. Empty bounds retain
  the pure exponential tails from the original implementation.
- **Do not change the existing per-block log line**
  (`{name} => Mined 1 block [{height}] {hash} ...`). Nothing parses it today, but it
  is the line users watch; the new `TIMING` line and the miner name in the existing
  line together give tests everything they need to correlate (greppable prefixes:
  `TIMING`, `Mined`).
- **`wait_for_height(other, ...)` stays per-block** even when the same miner wins
  twice in a row — it keeps the non-mining node synced so blocks never stack.
- The `mined` variable's `generate_to_address(1, addr)` call is unchanged; consecutive
  same-miner blocks need no special handling.

### 4. Wiring (`docker-compose.yml`)

In the `btc-simnet-mining-controller` service environment:

```yaml
- BLOCK_INTERVAL_MEAN_SECS=${BLOCK_INTERVAL_MEAN_SECS:-15}
- BLOCK_INTERVAL_MODE=${BLOCK_INTERVAL_MODE:-poisson}
- BLOCK_INTERVAL_MIN_SECS=${BLOCK_INTERVAL_MIN_SECS:-10}
- BLOCK_INTERVAL_MAX_SECS=${BLOCK_INTERVAL_MAX_SECS:-20}
- MINER_WEIGHTS=${MINER_WEIGHTS:-}
- MINING_RNG_SEED=${MINING_RNG_SEED:-}
```

There is exactly one place to edit: the service's `environment:` block (the string
`btc-simnet-mining-controller` also appears at line ~199, but that is the spammer's
`depends_on` entry — no changes there).

### 5. Env examples

- `.env.example`: document the renamed mean plus the optional timing bounds and
  stochastic variables.
- `.env.full.example`: same, in the mining-controller section (around line 94), with
  a short recipe block, e.g.:

```bash
# Default block pace: bounded exponential intervals from 10s to 20s
BLOCK_INTERVAL_MODE=poisson
BLOCK_INTERVAL_MEAN_SECS=15
BLOCK_INTERVAL_MIN_SECS=10
BLOCK_INTERVAL_MAX_SECS=20
# Optional 70/30 hashrate split
# MINER_WEIGHTS=70,30
# Reproducible run (replay a bug):
# MINING_RNG_SEED=42
```

### 6. Docs

- `docs/SETTINGS.md`: document the renamed mean, both optional bounds, the stochastic
  controls, clamp semantics, and the Poisson/spam interaction.
- **Poisson × full-block tuning interaction (document, do not "fix"):** SETTINGS.md
  advises keeping `BLOCK_INTERVAL_MEAN_SECS` above the spam cycle time so blocks come
  out full. Under Poisson timing, individual intervals routinely fall below the cycle
  time unless `BLOCK_INTERVAL_MIN_SECS` prevents it — the
  block after a short gap rides on the standing floor pool and whatever the previous
  cycle left in the mempool, so it may be partially filled. This is realistic
  (mainnet blocks after short gaps also draw down the backlog) and is expected
  behavior, not a bug.
- `README.md` line ~83: mermaid node says "bootstrap + round-robin mining" — update
  wording if weights are worth a mention there; line ~30's controller description
  likewise.

### 7. After shipping (repo conventions — mandatory)

- **Delete** item 1 from `docs/nice-to-have.md` (do not mark it done), renumber the
  remaining items, and fix any "N items" counts in that file's intro.
- Rebuild the image in the same compose project before testing:
  `docker compose build btc-simnet-mining-controller && docker compose up -d` —
  editing the Rust source does NOT change the running container (known staleness
  trap in this repo).
- Do **not** commit or stage anything — the user commits himself after manual
  testing.

## Verification plan

1. **Default regression (most important):** run with no timing or weight overrides. Logs
   must show strict Node 2/Node 3 alternation, targets in `[10,20]`, `TIMING` lines, and
   startup config `interval=poisson mean=15s, bounds=[10s, 20s], weights=alternate`.
2. **Unbounded Poisson distribution sanity:** `BLOCK_INTERVAL_MODE=poisson`,
   `BLOCK_INTERVAL_MEAN_SECS=5` with both bounds empty. Let ~100+ blocks accumulate,
   parse the `TIMING sampled interval` values from
   `docker compose logs btc-simnet-mining-controller`:
   - mean ≈ 5s (within ~±20% at n=100),
   - coefficient of variation ≈ 1 (exponential signature),
   - at least one sample < 1s and at least one > 15s.
3. **Bounds:** with mean 5, min 2 and max 10, raw samples still cross both boundaries,
   every target stays in `[2,10]`, and crossed samples produce targets exactly 2 or 10.
   In fixed mode the same bounds have no effect and every target remains the mean.
4. **Weighted selection:** `MINER_WEIGHTS=70,30`, ~200 blocks; count `Node 2 =>
   Mined` vs `Node 3 => Mined` lines — expect ≈140/60 (a 70/30 binomial at n=200 has
   σ≈6.5, so anything in ~127–153 for node2 passes). Confirm at least one same-miner
   streak of ≥3 exists (with 70/30 it's near-certain in 200 blocks).
5. **Edge weights:** `MINER_WEIGHTS=100,0` → every block Node 2; chain still
   advances and node3 stays synced (wait_for_height works).
6. **Seed replay:** two runs with `MINING_RNG_SEED=42` (and the reorg simulator +
   spammer idle or identical) → the sequence of `TIMING` samples and miner picks in
   the logs is identical.
7. **Fail-fast:** invalid mode, reversed/malformed bounds, zero mean, a Poisson mean
   outside the bounds (e.g. mean 60 with the default 10–20 clamp), malformed weights,
   and malformed seed each crash at startup with the expected message.
8. **Interplay smoke test:** poisson + weights + spammer + reorg simulator all on for
   a while; REORG/EXTERNAL detection lines still appear and mining continues (the
   loop body around `sync_view` is untouched, so this should be trivially fine).

Note on tooling: use `/usr/bin/docker` directly when inspecting output (CLI wrappers
in this environment alter output), and check cargo exit codes rather than trusting
printed output.

## Out of scope

- Difficulty adjustment / real hashing (regtest `generate` has no PoW).
- More than 2 miners (weights are positional node2,node3; generalizing to N miners
  is a separate feature).
- Any spammer changes (it is block-event-driven and needs none).
- Any reorg-simulator changes (the controller already mines on whatever tip the node
  reports; weighted races make reorg scenarios more interesting for free).
- Timing-based miner selection races (simulating two miners finding blocks nearly
  simultaneously) — the reorg simulator already covers competing-chain scenarios.

## Effort estimate

Small: one Rust file (PRNG + parsing + loop tweaks), plus wiring and docs. No new
dependencies or schema/protocol changes. The environment-variable rename requires
existing `.env` files to migrate from `BLOCK_INTERVAL_SECS` to
`BLOCK_INTERVAL_MEAN_SECS`.
