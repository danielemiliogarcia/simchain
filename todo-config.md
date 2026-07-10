# TODO — Centralized Config Module (tech debt)

Handoff plan for another agent. Self-contained. Can be done independently of, or
after, `todo.md` (logging/errors) — see "Interaction with todo.md" below.

## Goal

Resolve the third `## Tech debt` bullet in `docs/NICE-TO-HAVE.md`:

- [ ] use a config module to read env, validate, and serve configs

Build a **centralized, validated, globally-accessible** configuration layer for
the Rust code. Today env vars are read ad-hoc (`env_or(key, default)` scattered
across binaries with default string literals at each call site, plus raw
`env::var` parsing). Replace that with one config module that:

1. reads settings from the process environment (which docker-compose already
   populates from `.env`), optionally loading a local `.env` for bare
   `cargo run`;
2. **validates** — parses each value to its real type (int/float/bool/enum),
   checks ranges, and fails fast with a clear aggregated error listing every
   problem;
3. **serves** the parsed config to all Rust code **without threading it through
   function parameters** — a global singleton accessible from anywhere.

## The reference project has no config helper — craft our own

`/home/emi/SoftDev/FAIRGATE/BIT2/bit2-communication` loads config from
TOML/YAML via serde structs; it does **not** provide an env-based config loader.
So this is a from-scratch component. Still imitate the reference's *style*
(see `FAIRGATE_STANDARDS.md`): typed structs with `#[derive(Debug, Clone)]`,
`thiserror` for the error type, small focused modules (< ~150 lines), no
`.unwrap()`/`.expect()` in library code.

## Hard constraints — do not break

- **Docker compose and all non-Rust code keep reading settings from env exactly
  as they do now.** Do not move config into a TOML/YAML file, do not change how
  compose injects variables, do not rename existing env var keys. The `.env`
  file stays the single source of truth that both compose and the Rust config
  read. This module only changes the **Rust** side.
- **Global access, no parameter passing** — the maintainer explicitly wants to
  reach config "from everywhere without passing it as a parameter." Use a
  `static` / singleton (see design below).
- Preserve existing **default values** and existing **key names** exactly — the
  defaults currently live as the second arg to `env_or(...)`; move them into the
  config, don't change them. A running simnet with an existing `.env` must behave
  identically after this change.
- Edition stays **2021** (do not bump). `std::sync::OnceLock` is available and is
  the recommended singleton primitive — no extra crate needed for it.
- **No mainnet policy drift** (`AGENTS.md`): config only reads/validates existing
  knobs; do not add new bitcoind policy flags.
- **Do not commit or stage.** Maintainer manual-tests then commits himself.

## Current state

- `crates/simchain-common/src/lib.rs` has `env_or(key, default) -> String` and
  RPC client constructors. Everything is stringly-typed; no validation.
- Binaries parse env inline, e.g. `crates/spammer/src/main.rs` does
  `env::var("SPAM_FIXED_TXS_PER_BLOCK").or_else(|_| env::var("SPAM_TXS_PER_BLOCK"))`
  with manual `match` parsing and fallback chains. Reorg / mining-controller use
  `env_or` + local `.parse()`.
- **~36 distinct env keys** are read from Rust. Build the authoritative list
  yourself before coding — note the earlier inventory missed keys containing
  digits (`NODE1_RPC_URL`, `NODE2_RPC_URL`, `NODE3_RPC_URL`) because of a
  regex gap. Use:

  ```bash
  grep -rhoE 'env_or\("[A-Z0-9_]+"|env::var\("[A-Z0-9_]+"' crates --include='*.rs' \
    | grep -oE '"[A-Z0-9_]+"' | sort -u
  ```

  (Exclude the two test-only keys `SIMCHAIN_COMMON_SET_KEY` /
  `SIMCHAIN_COMMON_DEFINITELY_UNSET_KEY`.)

### Env surface, grouped by consumer

Confirm/complete this table from the grep above and by reading each `main.rs`.
Types/ranges are the validation targets.

| Group | Keys | Notes / validation |
|---|---|---|
| **Common / RPC** | `BTC_RPC_USER`, `BTC_RPC_PASS`, `NODE1_RPC_URL`, `NODE2_RPC_URL`, `NODE3_RPC_URL` | URLs must parse; user/pass required. Defaults today: user `foo`, pass `rpcpassword`, node URLs `http://btc-simnet-nodeN:18443`. |
| **Mining controller** | `BLOCK_INTERVAL_MODE`, `BLOCK_INTERVAL_MEAN_SECS`, `BLOCK_INTERVAL_MIN_SECS`, `BLOCK_INTERVAL_MAX_SECS`, `MINER_WEIGHTS`, `MINING_RNG_SEED`, `AUTO_REORG_EVERY_BLOCKS` | `MODE` is an enum (validate variants). min ≤ mean ≤ max. `MINER_WEIGHTS` is a structured string — validate its format. |
| **Reorg** | `REORG_MODE`, `REORG_DEPTH`, `REORG_NODE`, `REORG_NODE_RPC_PORT`, `REORG_WITNESS_NODE`, `REORG_WALLET_NAME`, `REORG_ADDS_NEW_TXS` | `MODE` enum. `DEPTH`/port are ints. |
| **Spammer** | `ENABLE_SPAM`, `ENABLE_SPAM_REPLACES`, `USE_RAW_TX_SPAM`, `FALLBACK_FEE`, `SPAM_FIXED_TXS_PER_BLOCK`, `SPAM_TXS_PER_BLOCK`, `SPAM_PER_MINER_PER_BLOCK`, `SPAM_SMALL_TXS_PER_BLOCK`, `SPAM_REPLACES_PER_MINER_PER_BLOCK`, `SPAM_FILL_BLOCK_RATIO`, `SPAM_FLOOR_POOL_TXS`, `SPAM_SENDMANY_OUTPUTS`, `SPAM_FANOUT_AUTO`, `SPAM_FANOUT_UTXOS`, `SPAM_TX_DATA_BYTES`, `SPAM_TX_DATA_MIN_BYTES`, `SPAM_TX_DATA_MAX_BYTES` | `ENABLE_*`/`USE_*`/`*_AUTO` are bools. `*_RATIO` in `0.0..=1.0`. Preserve the existing alias/fallback chains (e.g. `SPAM_FIXED_TXS_PER_BLOCK` → `SPAM_TXS_PER_BLOCK`). |

## Design

### Singleton

Use `std::sync::OnceLock` per config struct. Pattern:

```rust
use std::sync::OnceLock;

static SPAM_CONFIG: OnceLock<SpamConfig> = OnceLock::new();

impl SpamConfig {
    /// Parse + validate from the environment. Call once, early in `main`.
    pub fn init() -> Result<&'static Self, ConfigError> {
        let cfg = Self::from_env()?;      // parse + validate, aggregate errors
        Ok(SPAM_CONFIG.get_or_init(|| cfg))
    }

    /// Access anywhere after `init`. Panics if called before init — that is a
    /// programmer error, acceptable for a global (document it).
    pub fn global() -> &'static Self {
        SPAM_CONFIG.get().expect("SpamConfig::init() not called in main")
    }
}
```

Each binary calls `XConfig::init()?` as the first thing in `main` (after the
tracing subscriber if `todo.md` landed); every other function reads
`XConfig::global()` — no config parameter is threaded through.

### Module layout (recommended)

- `crates/simchain-common/src/config/` — shared home:
  - `error.rs` — `ConfigError` (`thiserror` enum): `Missing(key)`,
    `Invalid { key, value, cause }`, `OutOfRange { key, .. }`, and an
    `Aggregate(Vec<ConfigError>)` variant so **all** problems surface at once,
    not one-at-a-time.
  - `env.rs` — typed primitives built on `std::env::var`:
    `require(key) -> Result<String>`, `parse_or<T: FromStr>(key, default)`,
    `parse_bool(key, default)`, `parse_enum`, plus range validators. These
    replace `env_or`. Keep `env_or` temporarily or delete it once callers move.
  - `common.rs` — `CommonConfig` (RPC user/pass/URLs + `RPC_TIMEOUT_SECS`), with
    `init()` / `global()`.
- Tool-specific structs (`MiningConfig`, `ReorgConfig`, `SpamConfig`) live **in
  their own crate** (`crates/<tool>/src/config.rs`) built on the common
  primitives — this keeps `simchain-common` free of tool-specific knobs and
  matches "keep modules small and focused." (Alternative: put them all in
  `simchain-common`; only do that if you prefer one crate owning everything.)

### Local `.env` loading (optional convenience)

In docker, compose sets real env vars, so nothing is needed. For bare
`cargo run` on the host, load `.env` at the very top of `main` with **`dotenvy`**
(`dotenvy = "0.15"`, the maintained dotenv fork):

```rust
let _ = dotenvy::dotenv(); // ignore "file not found"; does NOT override real env vars
```

`dotenvy::dotenv()` does not overwrite variables already set in the environment,
so docker behavior is unchanged (compose-set vars win). Add `dotenvy` only to the
binaries, not to `simchain-common`.

## Plan

1. Add deps: `thiserror = "2"` to `simchain-common` (for `ConfigError`); add
   `dotenvy = "0.15"` to the three binaries (optional local `.env` load). No
   `[workspace.dependencies]` table — declare per-crate (`AGENTS.md`). Update
   `Cargo.lock` (CI runs `--locked`).
2. Build the primitives (`env.rs`) and `ConfigError` in `simchain-common`, with
   unit tests (the existing `env_or` tests show the pattern — set/remove a var
   and assert). Aggregate errors: parse every field, collect failures, return
   `ConfigError::Aggregate` if any.
3. Build `CommonConfig` (RPC) and migrate `create_client` callers to read URLs
   from it.
4. Per tool, add `config.rs` with the tool's struct + `init()/global()`. Move
   the current `env_or(default)` defaults verbatim into it. Preserve alias
   chains (e.g. `SPAM_FIXED_TXS_PER_BLOCK` → `SPAM_TXS_PER_BLOCK`).
5. Replace every inline `env::var` / `env_or` read in that binary with
   `XConfig::global().field`. Call `XConfig::init()?` at the top of `main`.
6. Remove now-dead `env_or` once no caller remains (or leave it if the primitives
   supersede it and nothing calls it — clippy dead-code will tell you).

## Interaction with todo.md

- `ConfigError` is a `thiserror` enum → if `todo.md` (errors plan) hasn't landed,
  this plan is the one that first adds `thiserror` to `simchain-common`. That's
  fine; the plans are compatible. Whichever lands first adds the dep.
- If both land, `main` order is: `dotenvy::dotenv()` → tracing subscriber →
  `XConfig::init()?` → run. `init()` returning `Result` composes cleanly with a
  `fn main() -> anyhow::Result<()>` via `?`.
- Validation failures should be surfaced via `tracing::error!` (if logging plan
  landed) or plain return-from-main otherwise.

## Verification (run from repo root)

```bash
cargo ba && cargo ca && cargo fac && cargo tt
```

All green. Behavior-equivalence check — with an existing `.env`, the three tools
must start and behave exactly as before. Also test validation fails loudly:

```bash
BLOCK_INTERVAL_MEAN_SECS=notanumber cargo run -p simchain-mining-controller
# expect a clear ConfigError naming the bad key, not a panic/backtrace
```

## Done criteria

- One config module; every Rust env read goes through it; no scattered
  `env::var`/`env_or(default)` at call sites.
- Values are typed + validated; bad input yields an aggregated, human-readable
  `ConfigError`.
- Config reachable via `XConfig::global()` with no parameter threading.
- Docker/compose and `.env` unchanged; existing simnet behaves identically.
- `cargo ba && cargo ca && cargo fac && cargo tt` green.
- **Remove the resolved bullet** "use config module to read env, and serve
  configs" from the `## Tech debt` section of `docs/NICE-TO-HAVE.md`. If it is
  the last bullet in that section (i.e. `todo.md` already removed the other two),
  also delete the empty `## Tech debt` heading and its `---` separator. (Project
  rule: delete shipped items, never mark them done.)
- Do **not** commit — hand the dirty tree back to the maintainer.
