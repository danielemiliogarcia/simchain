# AGENTS.md

Guidance for agents and contributors working in this repository.

## Repository structure

The Rust tools are members of a single Cargo workspace rooted at the repo top:

```
Cargo.toml                  # workspace root (members + resolver = "2")
Cargo.lock                  # committed — binaries want reproducible builds
.cargo/config.toml          # project-wide cargo aliases
docker/                     # docker build files and helper scripts
  bitcoin-node.Dockerfile   # local bitcoind image build
  tools.Dockerfile          # one builder stage, per-tool final targets
  build-bitcoin-image.sh    # local bitcoind image build helper
  entrypoint.sh             # bitcoind container entrypoint
scripts/                    # host-side helper scripts
  chainwatch.sh             # host RPC watcher
  simulate-reorg.sh         # convenience reorg wrapper
crates/
  simchain-common/          # shared helpers (RPC clients, config parsing)
  mining-controller/        # bootstrap + configurable mining
  reorg/                    # on-demand chain reorganizations
  spammer/                  # block-filling transaction spam
  scenario-engine/          # ordered declarative scenario orchestration
  control-plane/            # dashboard, versioned API, MCP, orchestration
  simchainctl/              # first-party HTTP client for humans and CI
```

`.dockerignore` intentionally remains at the repo root because Docker applies it
to the build context root (`.`), not to the directory containing the Dockerfile.

`.cargo/config.toml` lives at the repo root; Cargo discovers it by walking up the
directory tree, so the aliases work from any crate directory.

There is deliberately **no `[workspace.dependencies]` table**: each crate declares its
own dependencies so this workspace can be embedded under an upper-level workspace
without its version pins clashing with a shared table. A parent workspace must
exclude this directory to avoid nested-workspace errors:

```toml
[workspace]
exclude = ["path/to/simchain"]
```

`Cargo.lock` is committed on purpose (these are binaries for a reproducible test
network). Do not add it to any `.gitignore`.

## Project intent

- `crates/simchain-common` — the one home for helpers shared across tools (RPC client
  construction, config parsing/validation, logging). Put a helper here the moment a
  second tool needs it, rather than copy-pasting.
- `crates/control-plane` — the single public Simchain backend. Keep HTTP, MCP, CLI, and
  dashboard adapters over the same domain service layer; never add a second backend.
- `crates/simchainctl` — a thin control-plane API client. It must not call Docker or
  Bitcoin RPC directly.
- `crates/mining-controller`, `crates/reorg`, `crates/spammer`,
  `crates/scenario-engine` — worker/operation binaries, each a thin RPC driver or orchestrator
  over bitcoind. They must imitate mainnet **behavior**; do not add
  relay/mempool/capacity policy flags that diverge from mainnet.

## Commands

All `cargo` commands run from the repo root; target a single crate with `-p <name>`.

```bash
cargo ba            # build all targets (lib, bins, tests) — PREFER over cargo build
cargo bar           # same, release mode
cargo tt            # run tests serially (test --test-threads=1)
cargo ca            # clippy --all-targets -- -D warnings
cargo fa            # cargo fmt --all
cargo fac           # cargo fmt --all --check
```

`ba`, `bar`, `tt`, `ttr`, `ca`, `fa`, `fac` are aliases from `.cargo/config.toml`, not
standard Cargo commands.

**Prefer `cargo ba` over `cargo build`** — it compiles tests too, so a broken test
surfaces at build time.

Before committing, CI-equivalent local check:

```bash
cargo ba && cargo ca && cargo fac && cargo tt
```

CI runs the same jobs on every pull request (`.github/workflows/ci.yml`), but with
`--locked` added — so update `Cargo.lock` in the same change when you touch
dependencies, or CI fails. Note `--locked` cannot simply be appended to the `ca`/`tt`
aliases (their `--` would forward it to the lint/test harness); CI spells those
commands out (`cargo clippy --all-targets --locked -- -D warnings`,
`cargo test --locked -- --test-threads=1`).
