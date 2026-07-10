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

All containers join a single Docker network (`btc-simnet-network`). The three bitcoind
nodes form a full P2P mesh (`-addnode`, port 18444). The user lives on the host and
talks to **node1**, the non-mining full node, over RPC on `localhost:18443`, exactly like
talking to a 3rd-party production endpoint (node2's RPC on `localhost:28443` is also
exposed, for the "owned node" scenarios).

```mermaid
flowchart TB
    subgraph host["Host machine"]
        user["User / your tests<br/>external wallet, signs raw txs"]
        zmqc["ZMQ consumers<br/>LND / CLN / indexers / watchers"]
    end

    subgraph net["Docker network: btc-simnet-network"]
        subgraph mesh["bitcoind nodes — full P2P mesh (port 18444)"]
            n1["node1 — full node, never mines<br/>txindex, wallet disabled<br/>production-like endpoint"]
            n2["node2 — miner<br/>wallet enabled, owned node"]
            n3["node3 — miner<br/>not exposed to host"]
        end
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

    subgraph net["btc-simnet-network (tool profiles)"]
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

# Tear down (regtest keeps no volumes; the chain resets on next up)
docker compose --profile all-tools down
```

### Retuning a live chain

Change mining controller and spammer settings without restarting nodes; chain state preserved, only tool containers replaced.
Quickest way to experiment with block cadence, fee floor, and block fill on a live chain.

For full details and caveats, see [RETUNING.md](./docs/RETUNING.md).

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

With `mempool` or `all-tools`, browse the explorer at
[http://localhost:1080/](http://localhost:1080/) (port: `MEMPOOL_WEB_PORT`).


## Simulating reorgs

Forces chain reorgs by invalidating N blocks and mining N+1 replacements; orphaned txs fall back to mempool, new blocks rebuilt from live mempool.
Race-safe against mining controller; supports one-shot and continuous modes with configurable depth.

For full details, commands, and modes, see [REORGS.md](./docs/REORGS.md).

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

The three Rust tools live in a single Cargo workspace at the repo root, sharing one
`target/` dir, one dependency resolution, and one committed `Cargo.lock` so every build
of a given commit ships identical dependency versions.

| Path | Purpose |
|---|---|
| [crates/simchain-common](crates/simchain-common) | Shared helpers: RPC client construction (`create_client`) and env lookup (`env_or`), used by all three tools |
| [crates/mining-controller](crates/mining-controller) | Bootstraps the chain and drives configurable mining (`btc-simnet-mining-controller`) |
| [crates/spammer](crates/spammer) | Fills blocks with transactions (`btc-simnet-spammer`) |
| [crates/reorg](crates/reorg) | Forces chain reorganizations on demand (`btc-simnet-reorg`) |

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
- [SETTINGS.md](./docs/SETTINGS.md), every setting, its default and what it does.
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

To reset the chain from scratch, remove the containers instead:
`docker compose --profile all-tools down` (regtest keeps no volumes; everything resets
on the next `up`).
