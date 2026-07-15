# BTC Simchain

[![CI](https://github.com/danielemiliogarcia/simchain/actions/workflows/ci.yml/badge.svg)](https://github.com/danielemiliogarcia/simchain/actions/workflows/ci.yml) [![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](./LICENSE)

A regtest Bitcoin simulation network that tries to stay as close to mainnet reality as
regtest allows: several P2P-connected nodes, rotating miners, a non-mining full node as
the user endpoint, non-empty blocks, and simulated reorgs, all controlled from a `.env`
file.

## Intro

Blockchain regtest tool that helps write tests needing minimal changes to run on testnet/mainnet.
Three P2P-connected nodes, rotating miners, non-mining user endpoint, non-empty blocks, configurable reorgs.

For detailed component descriptions, see [INTRO.md](./docs/INTRO.md).

## Network topology

Traffic is split across two Docker networks. Only the three bitcoind nodes join
`btc-simnet-p2p`, where `node1-p2p`, `node2-p2p`, and `node3-p2p` form the full P2P
mesh on port 18444. Nodes and helper containers also join `btc-simnet-control` for RPC,
health checks, and explorer traffic. This separation lets P2P links be partitioned or
impaired without losing control access. The user talks to **node1** over RPC on
`localhost:18443`; node2's RPC is also exposed on `localhost:28443`.

```mermaid
flowchart TB
    subgraph host["Host machine"]
        user["User / your tests<br/>external wallet, signs raw txs"]
        zmqc["ZMQ consumers<br/>LND / CLN / indexers / watchers"]
    end

    subgraph mesh["btc-simnet-p2p — bitcoind full mesh (port 18444)"]
        n1["node1 — full node, never mines<br/>txindex, wallet disabled<br/>production-like endpoint"]
        n2["node2 — miner<br/>wallet enabled, owned node"]
        n3["node3 — miner<br/>not exposed to host"]
    end

    subgraph control["btc-simnet-control — RPC and helper traffic"]
        mc["mining-controller<br/>bootstrap + configurable mining"]
        sp["spammer<br/>fills blocks with txs"]
        rg["reorg simulator<br/>profile: reorg, on demand"]
    end

    %% Invisible waypoints pull the host arrows apart so each port label
    %% sits near the host boxes in open space instead of tangling with the
    %% other arrows.
    zmq1(( )):::waypoint
    rpc2a(( )):::waypoint
    rpc2(( )):::waypoint
    zmq2(( )):::waypoint

    user ==>|"RPC localhost:18443"| n1
    zmqc -.-|"ZMQ 28332-28336"| zmq1
    zmq1 -.-> n1

    user -.- rpc2a
    rpc2a -.-|"RPC localhost:28443"| rpc2
    rpc2 -.-> n2
    zmqc -.-|"ZMQ 38332-38336"| zmq2
    zmq2 -.-> n2

    n1 <-->|P2P| n2
    n1 <-->|P2P| n3
    n2 <-->|P2P| n3

    mc -->|"RPC: mine block"| n2
    mc -->|"RPC: mine block"| n3
    sp -->|"RPC: watch height"| n1
    sp -->|"RPC: raw spam + floor fills"| n2
    sp -->|"RPC: raw spam + floor fills"| n3
    rg -->|"RPC: invalidate + re-mine"| n3
    rg -.->|"witness poll"| n1

    classDef waypoint width:0px,height:0px,fill:none,stroke:none
```

With the `electrs` / `mempool` / `all-tools` [profiles](#profiles), the explorer stack
also joins the network and indexes the chain through node1:

```mermaid
flowchart LR
    browser["Browser<br/>localhost:1080"]
    electrum["Electrum clients<br/>localhost:60001"]

    subgraph net["btc-simnet-control (tool profiles)"]
        mweb["mempool-web"]
        mapi["mempool-api"]
        mdb["mempool-db<br/>MariaDB"]
        el["electrs"]
        n1["node1"]
    end

    browser --> mweb --> mapi
    electrum -.-> el
    mapi -->|"electrum :60001"| el
    mapi -->|"core RPC :18443"| n1
    mapi --> mdb
    el -->|"RPC :18443"| n1
```

## Configuration

Everything is driven by `.env`, and **every setting has a default**, the stack runs with
no `.env` file at all. To customize:

```bash
cp .env.example .env        # the most used settings (image, credentials, blocktime, spam)
# or, to tweak everything:
cp .env.full.example .env
```

Every setting (node image, credentials, host ports, fee policy, user address, block
interval, spam volume, reorg behavior, tool images/ports, explorer DB credentials) is
documented with its default in **[SETTINGS.md](./docs/SETTINGS.md)**.

### Choosing the bitcoin node image

By default the stack pulls the official registry image, no build step needed:

```bash
BTC_IMAGE=bitcoin/bitcoin:31.1   # default if unset
```

To use the locally built image instead (arch auto-detected; binaries are
checksum-verified and the SHA256SUMS file's GPG signature is checked against the
Bitcoin Core builder keys from
[bitcoin-core/guix.sigs](https://github.com/bitcoin-core/guix.sigs)):

```bash
./docker/build-bitcoin-image.sh           # builds simchainbitcoinnode:<BITCOIN_VERSION>
echo "BTC_IMAGE=simchainbitcoinnode:31.1" >> .env
```

`docker/build-bitcoin-image.sh` uses `BITCOIN_VERSION` from the environment or `.env`
(default 31.1). It only builds the bitcoin node image; the Rust tool images are built
by compose itself.

## How to run

```bash
docker compose --profile all-tools up -d
```

That's it (with the default registry image there is nothing to build). Useful follow-ups:

```bash
# Mining logs, find the banner with the funded user address
docker compose logs -ft btc-simnet-mining-controller

# Spammer logs
docker compose logs -ft btc-simnet-spammer

# Reorg simulator logs in auto mode (one-shot runs print to the terminal)
docker compose logs -ft btc-simnet-reorg

# bitcoind logs (node1 = the user-facing endpoint; same for node2/node3)
docker compose logs -ft btc-simnet-node1

# Everything at once
docker compose logs -ft

# Tear down; the chain persists on named volumes and resumes on the next up.
# Let it finish on its own -- see "Chain snapshots" for why force-killing it
# can cost you the chain.
docker compose --profile all-tools down

# Tear down AND wipe the chain (fresh bootstrap on the next up)
docker compose --profile all-tools down -v

# Or in one command: wipe + start a fresh chain (flags are passed to compose)
./scripts/fresh-chain.sh --profile all-tools
```

### Retuning a live chain

Change mining controller and spammer settings without restarting nodes; chain state preserved, only tool containers replaced.
Quickest way to experiment with block cadence, fee floor, and block fill on a live chain.

For full details and caveats, see [RETUNING.md](./docs/RETUNING.md).

### Chain snapshots

The node datadirs live on named volumes, so the chain survives `docker compose down`
and resumes on the next `up` (the mining controller detects the height and skips the
bootstrap); `down -v` wipes it for a fresh chain. On top of that,
`./scripts/snapshot.sh` archives and restores the **full chain state**, blocks, UTXO
set, miner wallets and mempool, so a bootstrapped, funded chain can be brought back in
seconds instead of re-mining and re-funding:

```bash
./scripts/snapshot.sh save mysnap                       # archive the running chain
./scripts/snapshot.sh restore mysnap                    # boot the simnet back at that state
./scripts/snapshot.sh list                              # what is saved
```

> **⚠️ Let `docker compose down`/`stop` finish on their own.** On shutdown bitcoind
> flushes the chainstate and dumps the mempool to `mempool.dat`; the compose file
> gives each node up to 5 minutes (`stop_grace_period: 300s`) to do that, and after a
> heavy spam run it can genuinely take a while. Force-killing the stack instead — a
> second `Ctrl+C`, `docker compose kill`, `docker rm -f` — skips the flush: the
> mempool is lost and the chainstate can be left unusable, and the only way back is a
> snapshot restore or a fresh chain (`./scripts/fresh-chain.sh`, wipes the volumes).
> If you want to resume or snapshot the chain later, always wait for the graceful stop.

A snapshot also records which services were running (tool profiles included), and
restore brings back exactly that shape — no `--profile` flags needed (passing compose
flags overrides it). Because the user's keys live outside the simnet, coins received
on the saved chain are still spendable after a restore with the same external keys.
Snapshots land in `./snapshots/` and are tied to the bitcoind image and wallet names
they were taken with (restore checks and refuses a mismatch).

Recipes for the common situations: **[SNAPSHOTS.md](./docs/SNAPSHOTS.md)**.

### Profiles

One compose file serves every combination via
[profiles](https://docs.docker.com/compose/how-tos/profiles/):

| Command | What comes up |
|---|---|
| `docker compose up` | basic simnet: 3 nodes + mining controller + spammer |
| `docker compose --profile basic up` | same as above (alias) |
| `docker compose --profile electrs up` | basic + electrs (Electrum RPC on 60001, HTTP on 3000) |
| `docker compose --profile mempool up` | basic + electrs + mempool.space explorer |
| `docker compose --profile all-tools up` | basic + all the tools above |
| `docker compose --profile control-plane up` | basic + the control plane (browser UI, HTTP API, MCP) |
| `SCENARIO_FILE=scenarios/reorg-during-sync.yml docker compose --profile scenario run --rm btc-simnet-scenario` | run one declarative scenario against the simnet, then exit |

With `mempool` or `all-tools`, browse the explorer at
[http://localhost:1080/](http://localhost:1080/) (port: `MEMPOOL_WEB_PORT`).

## Simchain control plane

The localhost control plane combines the dashboard, versioned API, and MCP endpoint
(profile: `control-plane`; `panel` is a temporary alias). During Phase 2 it retains the
existing Compose adapter only for spam, so it remains opt-in and excluded from
`all-tools` while that migration is unfinished. Mining policy and pause/resume already
use the mining worker's private API and never recreate its container:

```bash
docker compose --profile control-plane up -d --build
```

Open [http://localhost:8090/](http://localhost:8090/) (port: `CONTROL_PLANE_PORT`) to watch
chain height, block cadence, mempool depth and the fee histogram, and to change the
live-retunable mining/spam settings. Mining cadence and weights apply at a scheduler
safe point; spam still uses the transitional `.env`/Compose adapter. The nodes and the
chain are never touched, and a mixed apply rolls back transactionally. See
[RETUNING.md](./docs/RETUNING.md).

Everything the UI shows comes from the versioned localhost HTTP API
(`/api/v1/status`, `/api/v1/config`, `/api/v1/config/schema`). Mutating calls need the
bearer token stored at `.simchain-control/token` (gitignored, mode 0600):

```bash
curl -s localhost:8090/api/v1/status | jq .height
curl -s -X PATCH localhost:8090/api/v1/config \
  -H "Authorization: Bearer $(cat .simchain-control/token)" \
  -H "Content-Type: application/json" \
  -d '{"settings": {"SPAM_FILL_BLOCK_RATIO": "0.5"}}'
curl -s -X PUT localhost:8090/api/v1/mining/state \
  -H "Authorization: Bearer $(cat .simchain-control/token)" \
  -H "Content-Type: application/json" \
  -d '{"state": "paused"}'
```

The same operations are exposed over MCP (streamable HTTP) at
`http://localhost:8090/mcp`, so coding agents can inspect and retune the simnet
directly. Register it in Claude Code with:

```bash
claude mcp add --transport http simchain-control-plane \
  "http://localhost:8090/mcp" \
  --header "Authorization: Bearer $(cat .simchain-control/token)"
```

The CLI uses the same API and service operations:

```bash
cargo run -p simchainctl -- status
cargo run -p simchainctl -- config show --json
cargo run -p simchainctl -- mining pause
cargo run -p simchainctl -- mining resume
```

## Scenarios

Reproduce an ordered chain history from YAML after the simnet has bootstrapped:

```bash
docker compose up -d
SCENARIO_FILE=scenarios/reorg-during-sync.yml \
  docker compose --profile scenario run --rm --build btc-simnet-scenario
```

The one-shot engine waits for height 204, runs each step in order, stops at the first
failure, and exits with the scenario result. It supports height waits, sleeps, mining
pause/resume, manual mining, reorgs, one-shot wallet bursts, and deterministic network
partitions. The container mounts `docker.sock`, so the profile is intentionally opt-in
and excluded from `all-tools`. Schema, cleanup behavior, and all shipped examples are in
[SCENARIOS.md](./docs/SCENARIOS.md).

## Partitions and P2P latency

After bootstrap reaches height 204, create a deterministic organic fork by isolating one
miner, mining both branches, healing the P2P network, and waiting for the longer branch
to win everywhere:

```bash
./scripts/partition.sh run btc-simnet-node3 --main-blocks 3 --isolated-blocks 4
./scripts/partition.sh disconnect btc-simnet-node3
./scripts/partition.sh heal btc-simnet-node3
./scripts/partition.sh status
```

The `run` command pauses and restores the mining controller and, unless
`--keep-spammer` is passed, the spammer. Manual `disconnect` and `heal` do not manage
either service. Add latency or loss to only a node's P2P interface with the one-shot
netem helper:

```bash
./scripts/netem.sh apply btc-simnet-node3 --delay-ms 500 --loss-pct 1
./scripts/netem.sh status btc-simnet-node3
./scripts/netem.sh clear btc-simnet-node3
```

Netem state is ephemeral and clears when the target node is recreated or restarted.
Operational recipes and guardrails are in [RUNBOOK.md](./docs/RUNBOOK.md).


## Simulating reorgs

Forces chain reorgs by invalidating N blocks and mining N+1 replacements; orphaned txs fall back to mempool, new blocks rebuilt from live mempool.
Race-safe against mining controller; supports one-shot and continuous modes with configurable depth.

For full details, commands, and modes, see [REORGS.md](./docs/REORGS.md).


## Partitions and P2P latency

Isolates one miner from the P2P mesh (RPC stays up), mines competing branches on both
sides, then heals so the longer branch wins everywhere: an organic reorg caused by the
real mechanism (a partition), unlike the administrative reorg simulator below.
`degrade.sh` makes a node slower and/or lossy for N seconds or blocks (auto-restored);
`netem.sh` underneath gives fine control. P2P traffic only — block/tx propagation
becomes observable, RPC stays clean.

For commands, manual walkthroughs, and caveats, see
[PARTITIONS.md](./docs/PARTITIONS.md).


## ZMQ notifications

node1 and node2 publish all five bitcoind ZMQ topics (`rawblock`, `rawtx`, `hashblock`,
`hashtx`, `sequence`): node1 on host ports 28332-28336, node2 on 38332-38336 (all
remappable, see [SETTINGS.md](./docs/SETTINGS.md)). Anything that consumes bitcoind ZMQ
(LND/CLN, indexers, custody watchers) can point at the simnet, and reorg delivery can be
exercised with the reorg simulator. Smoke test (needs `pip install pyzmq`):

```bash
python3 -c "
import zmq
s = zmq.Context().socket(zmq.SUB)
s.connect('tcp://127.0.0.1:28332')      # node1 rawblock
s.setsockopt_string(zmq.SUBSCRIBE, '')
topic, body, seq = s.recv_multipart()   # blocks until the next block is mined
print(topic, len(body), 'bytes')
"
```

## Repository structure

The four Rust tools live in a single Cargo workspace at the repo root, sharing one
`target/` dir, one dependency resolution, and one committed `Cargo.lock` so every build
of a given commit ships identical dependency versions.

| Path | Purpose |
|---|---|
| [crates/simchain-common](crates/simchain-common) | Shared helpers: RPC clients, config parsing, logging, and burn addresses, used across the four tools |
| [crates/mining-controller](crates/mining-controller) | Bootstraps the chain and drives configurable mining (`btc-simnet-mining-controller`) |
| [crates/spammer](crates/spammer) | Fills blocks with transactions (`btc-simnet-spammer`) |
| [crates/reorg](crates/reorg) | Forces chain reorganizations on demand (`btc-simnet-reorg`) |
| [crates/scenario-engine](crates/scenario-engine) | Executes ordered YAML scenarios (`btc-simnet-scenario`) |

`Cargo.lock` is committed on purpose: these are binaries for a reproducible test
network, so the lockfile is tracked (unlike a library crate, which would leave it to
the consumer).

### Embedding under a parent workspace

The workspace is deliberately embeddable. There is **no `[workspace.dependencies]`
table** — each crate declares its own dependencies, so a parent workspace's version
pins can never conflict with a shared table here. If you embed this repo inside an
upper-level Cargo workspace, that workspace must exclude this directory to avoid
nested-workspace errors:

```toml
[workspace]
exclude = ["path/to/simchain"]
```

## Documents

- [INTRO.md](./docs/INTRO.md), detailed component descriptions and project objective.
- [RETUNING.md](./docs/RETUNING.md), how to retune mining cadence, fee floor, and block fill on a live chain.
- [REORGS.md](./docs/REORGS.md), simulating chain reorganizations, commands, and modes.
- [PARTITIONS.md](./docs/PARTITIONS.md), network partitions and P2P latency: organic
  reorgs, double-spend windows, propagation lag.
- [SCENARIOS.md](./docs/SCENARIOS.md), declarative scenario schema, execution, and examples.
- [SETTINGS.md](./docs/SETTINGS.md), every setting, its default and what it does.
- [SNAPSHOTS.md](./docs/SNAPSHOTS.md), chain snapshot/restore cookbook: concrete
  commands for the common situations.
- [snapshot-restore-plan.md](./docs/snapshot-restore-plan.md), chain snapshot/restore
  design, rationale and usage details.
- [NICE-TO-HAVE.md](./docs/NICE-TO-HAVE.md), all limitations, future enhancements and
  proposed features with rationale and implementation plans.
- [RUNBOOK.md](./docs/RUNBOOK.md), handy `bitcoin-cli` one-liners against the simnet.

## Limitations and future enhancements

All known limitations, future enhancements and proposed features live in
[NICE-TO-HAVE.md](./docs/NICE-TO-HAVE.md).

### Local development

All `cargo` commands run from the repo root. Project aliases live in
[.cargo/config.toml](.cargo/config.toml) (Cargo discovers it by walking up from any
crate directory):

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs `cargo ba`, clippy
(`-D warnings`), `cargo fmt --check`, and the test suite on every pull request, all
with `--locked` so a stale `Cargo.lock` fails the build. The three tool Docker images
build from one shared [docker/tools.Dockerfile](docker/tools.Dockerfile) (one builder
stage, three targets), also with `--locked`.

# Trouble shotting

Stopping the containers (`docker compose stop`) and starting them again used to crash
the mining controller with:

```
JsonRpc(Rpc(RpcError { code: -4, message: "Wallet file verification failed. Failed to create database path '/home/bitcoin/.bitcoin/regtest/wallets/node2'. Database already exists.", data: None }))
```

Fixed: the controller now loads the existing wallets and skips the funding sequence when
the chain is already bootstrapped (height >= 204), so `stop`/`start` resumes cleanly
where it left off.

To reset the chain from scratch, remove the containers **and the chain volumes**:
`docker compose --profile all-tools down -v` (a plain `down` keeps the named volumes,
so the chain resumes on the next `up`).
