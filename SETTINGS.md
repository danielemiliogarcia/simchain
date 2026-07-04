# Simchain Settings Reference

Every setting is read from `.env` by `docker-compose.yml`, and **every one has a
default**, so a missing variable (or no `.env` file at all) still works.

- `.env.example`, short template with the most used settings.
- `.env.full.example`, complete template with everything below.

```bash
cp .env.example .env        # everyday version
cp .env.full.example .env   # everything tweakable
```

## Bitcoin node image

| Variable | Default | Description |
|---|---|---|
| `BTC_IMAGE` | `bitcoin/bitcoin:29.0` | Docker image used by the 3 nodes. Default is pulled from the registry (no build needed). Set `simchainbitcoinnode:29.0` to use the locally built image (`./build.sh`). |
| `BITCOIN_VERSION` | `29.0` | Bitcoin Core version downloaded by `./build.sh` when building the local image. Not used by compose. |

## RPC credentials

Shared by all nodes and every tool (mining controller, spammer, reorg, electrs, explorer).

| Variable | Default | Description |
|---|---|---|
| `BTC_RPC_USER` | `foo` | RPC username. |
| `BTC_RPC_PASS` | `rpcpassword` | RPC password. |

> **Security note:** credentials are passed in plaintext on the bitcoind command line
> and are visible via `docker inspect` / `ps`. That is fine for this dev tool (throwaway
> regtest coins, private network), but do NOT replicate the pattern in production: use
> `-rpcauth` (salted hash) plus a proper secrets mechanism (Docker/Compose secrets,
> Kubernetes Secrets or a vault) instead of environment variables in compose files.

## Host port mappings

| Variable | Default | Description |
|---|---|---|
| `NODE1_RPC_PORT` | `18443` | Host port for node1 RPC (production-like endpoint). |
| `NODE1_P2P_PORT` | `18444` | Host port for node1 P2P. |
| `NODE2_RPC_PORT` | `28443` | Host port for node2 RPC (owned wallet node). |
| `NODE2_P2P_PORT` | `28444` | Host port for node2 P2P. |

Node3 is intentionally not exposed to the host.

## Node policy

The three fee settings look similar but act at different points of a transaction's life:

- **`MIN_RELAY_TX_FEE`** (`-minrelaytxfee`, BTC/kvB) is the **node's floor**: the minimum
  feerate for the node to accept a transaction into its mempool and relay it to peers.
  A transaction paying below this is rejected on arrival, whoever sends it.
- **`FALLBACK_FEE`** (`-fallbackfee`, BTC/kvB) is the **wallet's guess**: the feerate the
  wallet uses when fee estimation has no data, which is always the case on a fresh
  regtest chain. Without it `sendtoaddress` fails with "Fee estimation failed". Keep it
  at or above `MIN_RELAY_TX_FEE`, or the wallet creates transactions that its own node
  refuses to relay.
- **`MAX_TX_FEE`** (`-maxtxfee`, whole BTC; an absolute amount, not a rate) is the
  **wallet's safety cap**: any wallet transaction that would pay more total fee than this
  aborts. It is set absurdly high here so spam volume never trips it (the mainnet
  default is 0.1 BTC).

| Variable | Default | Description |
|---|---|---|
| `MIN_RELAY_TX_FEE` | `0.00001` | Node mempool/relay floor (feerate, BTC/kvB). |
| `FALLBACK_FEE` | `0.0001` | Wallet feerate when estimation has no data (BTC/kvB). |
| `MAX_TX_FEE` | `10000000` | Wallet cap on the total fee of one tx (whole BTC). |
| `NODE1_DISABLE_WALLET` | `1` | node1 has no wallet by default: it mimics a 3rd-party production endpoint with no hot wallet online, so the user manages keys externally and submits signed raw transactions. Set `0` to enable the wallet. |

## Mining controller

| Variable | Default | Description |
|---|---|---|
| `USER_ADDRESS` | `bcrt1qtmjq...tf3rr` | Address funded at startup with 2 coinbase UTxOs of 50 BTC (matured). Generate your own, see the helper gists linked in `.env.full.example`. |
| `BLOCK_INTERVAL_SECS` | `15` | Seconds between blocks; miners (node2/node3) alternate. |
| `NODE2_WALLET_NAME` | `node2` | Wallet created on node2 by the controller, also used by the spammer. |
| `NODE3_WALLET_NAME` | `node3` | Wallet created on node3 by the controller, also used by the spammer. |

## Spammer

| Variable | Default | Description |
|---|---|---|
| `ENABLE_SPAM` | `true` | Spam transactions after each block so blocks are not empty. |
| `SPAM_PER_MINER_PER_BLOCK` | `50` | Txs per miner per block (2 miners → up to 2x this per block). Excess waits in the mempool. |
| `SPAM_FANOUT_UTXOS` | `50` | On startup the spammer splits each wallet into this many UTXOs. The mempool caps unconfirmed chains at 25 txs, so without the split a wallet can never place more than 25 txs per block. `0` disables. |

## Reorg simulator (profile `reorg`)

| Variable | Default | Description |
|---|---|---|
| `REORG_DEPTH` | `3` | How many blocks to orphan per reorg. CLI argument overrides it: `./simulate-reorg.sh 5`. |
| `REORG_MODE` | `once` | `once` = single reorg then exit. `auto` = reorg every `AUTO_REORG_EVERY_BLOCKS`. |
| `AUTO_REORG_EVERY_BLOCKS` | `20` | Auto mode cadence (x); must be greater than `REORG_DEPTH` (y). |
| `REORG_NODE` | `btc-simnet-node3` | Node used to fork the chain (a hidden miner is realistic). |
| `REORG_MINE_ADDRESS` | `bcrt1qtmjq...tf3rr` | Address receiving the replacement block rewards. |
| `REORG_INJECT_TXS` | `5` | If the orphaned blocks carried no txs, send this many wallet txs before mining replacements so they are not empty. `0` disables. |

Orphaned transactions return to the mempool automatically and are re-mined into the
replacement blocks, so reorged blocks carry the same real transactions as the old chain.

## Tools: electrs (profiles `electrs`, `mempool`, `all-tools`)

| Variable | Default | Description |
|---|---|---|
| `ELECTRS_IMAGE` | `mempool/electrs:v3.3.0` | electrs image. |
| `ELECTRS_ELECTRUM_PORT` | `60001` | Host port for the Electrum RPC. |
| `ELECTRS_HTTP_PORT` | `3000` | Host port for the esplora-style HTTP API. |

## Tools: mempool.space explorer (profiles `mempool`, `all-tools`)

| Variable | Default | Description |
|---|---|---|
| `MEMPOOL_FRONTEND_IMAGE` | `mempool/frontend:v3.3.1` | Explorer web frontend image. |
| `MEMPOOL_BACKEND_IMAGE` | `mempool/backend:v3.3.1` | Explorer API backend image. |
| `MEMPOOL_WEB_PORT` | `1080` | Host port for the explorer UI (http://localhost:1080/). |
| `MARIADB_IMAGE` | `mariadb:10.5.8` | Database image for the explorer. |
| `MEMPOOL_DB_USER` | `mempool` | Explorer DB user. |
| `MEMPOOL_DB_PASS` | `mempool` | Explorer DB password. |
| `MEMPOOL_DB_ROOT_PASS` | `admin` | Explorer DB root password. |
