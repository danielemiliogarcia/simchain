# Contributing to BTC Simchain

Bug reports, documentation, tests, reviews, and code contributions are welcome.

This document is the contributor-facing entry point. Deeper repository conventions
(directory layout, per-crate responsibilities, naming, logging) live in
[AGENTS.md](./AGENTS.md), and every runtime setting is documented in
[docs/SETTINGS.md](./docs/SETTINGS.md).

- [Before you start](#before-you-start)
- [Project intent and hard constraints](#project-intent-and-hard-constraints)
- [Development environment](#development-environment)
- [Development workflow](#development-workflow)
- [Testing your change](#testing-your-change)
- [Documentation expectations](#documentation-expectations)
- [Commit and pull request conventions](#commit-and-pull-request-conventions)
- [Reporting bugs](#reporting-bugs)
- [Proposing features](#proposing-features)
- [Security issues](#security-issues)
- [License of contributions](#license-of-contributions)

## Before you start

For a new feature or a broad behavioral change, please
[open an issue](https://github.com/danielemiliogarcia/simchain/issues) first, so the
use case and its effect on mainnet fidelity can be agreed before implementation.
Small, self-contained fixes can go directly to a pull request.

Known limitations and already-scoped proposals live in
[docs/NICE-TO-HAVE.md](./docs/NICE-TO-HAVE.md). Check it before proposing something
new: the rationale and an implementation sketch may already be there.

Read [Scope and non-goals](./README.md#scope-and-non-goals) first. Changes that push
the project toward being a consensus, miner, or network-decentralization testbed are
out of scope regardless of implementation quality.

## Project intent and hard constraints

These are the constraints most likely to get an otherwise good pull request rejected:

- **Imitate mainnet behavior.** Do not add relay, mempool, or capacity policy that
  diverges from Bitcoin Core's mainnet defaults. Miner-template settings that move the
  simnet *toward* mainnet behavior are fine; policy knobs that make the simnet accept
  what mainnet would reject are not.
- **One backend.** `crates/control-plane` is the single public Simchain backend. HTTP,
  MCP, CLI, and dashboard adapters all sit over the same domain service layer. Never
  add a second backend.
- **Control-plane trust boundary.** The control plane must never gain a Docker socket,
  Docker CLI, repository bind mount, or process-lifecycle executor. This is asserted in
  CI by `./scripts/check-compose-security.sh` and `./scripts/check-docker-images.sh`.
- **`simchainctl` is a thin API client.** It must not call Docker or Bitcoin RPC
  directly.
- **`network-agent` stays private.** Lease/TTL bounded, authenticated, and unreachable
  from host ports or public networks.
- **Share helpers early.** Put a helper in `crates/simchain-common` as soon as a second
  tool needs it, rather than copy-pasting it.

## Development environment

Requirements:

- A stable Rust toolchain (CI uses `dtolnay/rust-toolchain@stable`) with `rustfmt` and
  `clippy`.
- Docker with Compose v2 and Buildx, for anything that touches the stack.

The Rust tools are members of one Cargo workspace rooted at the repo top. All `cargo`
commands run from the repo root; target a single crate with `-p <name>`. Project
aliases live in [.cargo/config.toml](./.cargo/config.toml) — Cargo discovers it by
walking up from any crate directory.

| Alias | Expands to |
| --- | --- |
| `cargo ba` | `cargo test --no-run --all-targets --benches` (build lib, bins, tests) |
| `cargo bar` | same, release mode |
| `cargo tt` | `cargo test -- --test-threads=1` |
| `cargo ttr` | same, release mode |
| `cargo ca` | `cargo clippy --all-targets -- -D warnings` |
| `cargo fa` | `cargo fmt --all` |
| `cargo fac` | `cargo fmt --all --check` |

Prefer `cargo ba` over `cargo build`: it compiles tests too, so a broken test surfaces
at build time.

## Development workflow

1. Fork the repository and branch off `master`.
2. Make the change, keeping it focused on one concern.
3. Format and run the same checks CI runs:

   ```bash
   cargo fa
   cargo ba && cargo ca && cargo fac && cargo tt
   ```

4. If the change touches Compose, Dockerfiles, or shell scripts, also run:

   ```bash
   docker compose config --quiet
   ./scripts/check-compose-security.sh
   ./scripts/check-docker-images.sh
   ```

5. If dependencies changed, commit the updated `Cargo.lock`.
6. Open a pull request against `master` and fill in the template.

CI ([.github/workflows/ci.yml](./.github/workflows/ci.yml)) runs `cargo ba`, clippy
with `-D warnings`, `cargo fmt --check`, and the test suite on every pull request, all
with `--locked` so a stale `Cargo.lock` fails the build. It also renders the Compose
trust boundary, builds every final image target, and inspects the control-plane root
filesystem for forbidden lifecycle tooling.

Note that `--locked` cannot simply be appended to the `ca`/`tt` aliases — their `--`
would forward it to the lint/test harness — so CI spells those commands out.

## Testing your change

Automated checks are the floor, not the ceiling. Rust unit and integration tests run
serially (`--test-threads=1`) because several of them drive shared state.

Changes that affect runtime behavior should also be exercised against a live stack.
[docs/RUNBOOK.md](./docs/RUNBOOK.md) has `bitcoin-cli` one-liners against the simnet,
and the helper scripts in `scripts/` cover the common maneuvers (reorgs, partitions,
degradation, snapshots, spam bursts). List in the pull request which of these you ran.

After editing tool source, rebuild the images in the *same* Compose project before
retesting — a stale image will happily contradict the source you just changed:

```bash
docker compose --profile all-tools up -d --force-recreate --build
```

## Documentation expectations

Update documentation in the same pull request as the behavior change:

- New or changed settings → [docs/SETTINGS.md](./docs/SETTINGS.md), plus `.env.example`
  and `.env.full.example`.
- New scenario schema surface → [docs/SCENARIOS.md](./docs/SCENARIOS.md).
- New or changed control-plane API → [docs/CONTROL_PLANE.md](./docs/CONTROL_PLANE.md),
  and [docs/MCP.md](./docs/MCP.md) if the MCP surface changes.
- Anything a user would hit at runtime → the relevant `docs/` page and, if it belongs
  in the overview, the [README](./README.md).

A shipped item is removed from [docs/NICE-TO-HAVE.md](./docs/NICE-TO-HAVE.md), not
marked done.

## Commit and pull request conventions

- Keep pull requests focused. One concern per pull request; unrelated cleanups belong
  in their own.
- Explain both *what* changed and *why*. The "why" is the part reviewers cannot
  reconstruct from the diff.
- Link the relevant issue.
- List the automated checks and manual scenarios you ran.
- Rebase or merge `master` if the branch has drifted; CI tests the merge result.

## Reporting bugs

Open a [bug report](https://github.com/danielemiliogarcia/simchain/issues/new/choose)
and include:

- What you ran (exact command, profile, and scenario file if any).
- What you expected versus what happened.
- Relevant logs (`docker compose logs <service>`) and the chain height/state at the
  time.
- Your Docker and Compose versions, and host OS.

## Proposing features

Open a feature request describing the *application-testing use case* first, then the
proposed mechanism. State explicitly how the change affects mainnet fidelity and
whether it stays inside [Scope and non-goals](./README.md#scope-and-non-goals).


## License of contributions

By submitting a contribution, you agree that it may be distributed under the project's
[`GPL-3.0-or-later`](./LICENSE) license.
