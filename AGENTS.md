# AGENTS.md

Guidance for agents and contributors working in this repository.

## Repository structure

The three Rust tools are members of a single Cargo workspace rooted at the repo top:

```
Cargo.toml                  # workspace root (members + resolver = "2")
Cargo.lock                  # committed — binaries want reproducible builds
.cargo/config.toml          # project-wide cargo aliases
tools.Dockerfile            # one builder stage, three final targets
crates/
  simchain-common/          # shared helpers (create_client, env_or)
  mining-controller/        # bootstrap + configurable mining
  reorg/                    # on-demand chain reorganizations
  spammer/                  # block-filling transaction spam
```

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
  construction, env lookup). Put a helper here the moment a second tool needs it,
  rather than copy-pasting.
- `crates/mining-controller`, `crates/reorg`, `crates/spammer` — the three binaries,
  each a thin RPC driver over bitcoind. They must imitate mainnet **behavior**; do not
  add relay/mempool/capacity policy flags that diverge from mainnet.

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
