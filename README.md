# BTC Simchain

A regtest Bitcoin simulation network that tries to stay as close to mainnet reality as
regtest allows: several P2P-connected nodes, rotating miners, a non-mining full node as
the user endpoint, non-empty blocks, and simulated reorgs, all controlled from a `.env`
file.

## Intro

The objective of this project is to be a tool that helps the user write blockchain
regtest tests in such a way that the code later needs only minimal modifications (or only
configuration, in the best case) to switch to testnet or mainnet.

The network consists of 3 well-connected nodes plus helper containers:

- **Node 1 `btc-simnet-node1`**, exposed to the host (RPC 18443). Simulates a production
  endpoint (`-txindex`, `-disablewallet=1`): like most 3rd-party production nodes there is
  no hot wallet online, so you manage your own keys in an external wallet, obtain the
  outpoints of your addresses' UTxOs and submit externally signed raw transactions; mining
  is not under your control. It never mines. Set `NODE1_DISABLE_WALLET=0` in `.env` if you
  need a wallet on it. Publishes all ZMQ topics on host ports 28332-28336
  (see [ZMQ notifications](#zmq-notifications)).
- **Node 2 `btc-simnet-node2`**, exposed to the host (RPC 28443). Simulates an owned node
  with internal wallet enabled, useful to stack an ordinals wallet or any layer-2 node on
  top that needs internal wallet management. Publishes all ZMQ topics on host ports
  38332-38336, so ZMQ consumers like LND/CLN can use it as their bitcoind backend. This
  node is a miner.
- **Node 3 `btc-simnet-node3`**, NOT exposed to the host. Simulates a node connected via
  p2p but inaccessible to the user. This node is a miner.
- **Mining controller `btc-simnet-mining-controller`**, bootstraps the chain: block 1
  goes to node2's wallet, block 2 to node3's wallet (so each miner has a mature reward to
  fund the spammer), blocks 3 and 4 fund the user address (2 UTxOs of 50 BTC = 100 BTC),
  then 100 more blocks are mined so all four coinbases mature (bootstrap ends at height
  104). After that it asks each miner to mine 1 block in a round-robin manner every
  `BLOCK_INTERVAL_SECS`. Stop this container after funding if you want to control mining
  manually.
- **Spammer `btc-simnet-spammer`**, spams `SPAM_PER_MINER_PER_BLOCK` transactions from
  each miner per block (2x total per block), so blocks are not empty. On startup it waits
  for the wallet funds to mature and splits them into `SPAM_FANOUT_UTXOS` independent
  UTXOs, otherwise the 25-tx unconfirmed-chain mempool limit would cap spam at 25 txs
  per wallet per block. If you spam many
  transactions, some may stay in the mempool and join the next batch, tune the settings
  to achieve the scenario you need, or disable with `ENABLE_SPAM=false`. With
  `ENABLE_SPAM_REPLACES=true` every spam tx signals RBF and a few per batch get
  fee-bumped, so the mempool carries real BIP125 replacements (see SETTINGS.md).
- **Reorg simulator `btc-simnet-reorg`** *(profile `reorg`, on demand)*, a Rust tool
  (same stack as the other tools, pure RPC calls) that forces chain reorganizations.
  See [Simulating reorgs](#simulating-reorgs).
- **Tools** *(profiles)*, [mempool.space](https://github.com/mempool/mempool) explorer
  and/or [electrs](https://github.com/mempool/electrs). See [Profiles](#profiles).

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
documented with its default in **[SETTINGS.md](./SETTINGS.md)**.

### Choosing the bitcoin node image

By default the stack pulls the official registry image, no build step needed:

```bash
BTC_IMAGE=bitcoin/bitcoin:29.0   # default if unset
```

To use the locally built image instead (arch auto-detected; binaries are
checksum-verified and the SHA256SUMS file's GPG signature is checked against the
Bitcoin Core builder keys from
[bitcoin-core/guix.sigs](https://github.com/bitcoin-core/guix.sigs)):

```bash
./build-bitcoin.sh                        # builds simchainbitcoinnode:<BITCOIN_VERSION>
echo "BTC_IMAGE=simchainbitcoinnode:29.0" >> .env
```

`build-bitcoin.sh` reads `BITCOIN_VERSION` from `.env` (default 29.0). It only builds
the bitcoin node image; the Rust tool images are built by compose itself.

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

## ZMQ notifications

node1 and node2 publish all five bitcoind ZMQ topics (`rawblock`, `rawtx`, `hashblock`,
`hashtx`, `sequence`): node1 on host ports 28332-28336, node2 on 38332-38336 (all
remappable, see [SETTINGS.md](./SETTINGS.md)). Anything that consumes bitcoind ZMQ
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

## Simulating reorgs

The reorg simulator (a Rust container using only bitcoind RPC calls) invalidates the last
*N* blocks on a miner node and mines *N+1* replacements, so the new chain is strictly
longer and **the whole network reorgs to it**. Transactions from the orphaned blocks fall
back to the mempool and are re-mined into the replacement blocks (same txids), like the
winning chain of a real reorg, so reorged blocks are not empty. Only if the orphaned
blocks carried no txs (e.g. `ENABLE_SPAM=false`), it injects `REORG_INJECT_TXS` fresh
wallet transactions per empty replacement block. It prints each block's hash and tx
count before/after plus a replaced-blocks summary.

The reorg is race-safe against the mining controller: after mining the replacements the
tool polls a witness node (`REORG_WITNESS_NODE`, default node1) and, if the miners kept
extending the old chain in the meantime, mines extra blocks until the network adopts the
new chain.

One-shot (container runs, reorgs, dies):

```bash
./simulate-reorg.sh 3
# equivalent to:
docker compose run --rm btc-simnet-reorg 3     # depth defaults to REORG_DEPTH (3)
```

Continuous, every `AUTO_REORG_EVERY_BLOCKS` (x) blocks, reorg `REORG_DEPTH` (y) blocks,
with x > y enforced:

```bash
REORG_MODE=auto docker compose --profile reorg up btc-simnet-reorg
```

Tune `REORG_DEPTH`, `AUTO_REORG_EVERY_BLOCKS`, `REORG_NODE`, `REORG_MINE_ADDRESS`,
`REORG_INJECT_TXS`, `REORG_WALLET_NAME` and `REORG_WITNESS_NODE` in `.env`
(see [SETTINGS.md](./SETTINGS.md)).

## Documents

- [SETTINGS.md](./SETTINGS.md), every setting, its default and what it does.
- [nice-to-have.md](./nice-to-have.md), all limitations, future enhancements and
  proposed features with rationale and implementation plans.
- [runbook.txt](./runbook.txt), handy `bitcoin-cli` one-liners against the simnet.

## Limitations and future enhancements

All known limitations, future enhancements and proposed features live in
[nice-to-have.md](./nice-to-have.md).

# Trouble shotting

Stopping the containers (`docker compose stop`) and starting them again used to crash
the mining controller with:

```
JsonRpc(Rpc(RpcError { code: -4, message: "Wallet file verification failed. Failed to create database path '/home/bitcoin/.bitcoin/regtest/wallets/node2'. Database already exists.", data: None }))
```

Fixed: the controller now loads the existing wallets and skips the funding sequence when
the chain is already bootstrapped (height >= 104), so `stop`/`start` resumes cleanly
where it left off.

To reset the chain from scratch, remove the containers instead:
`docker compose --profile all-tools down` (regtest keeps no volumes; everything resets
on the next `up`).
