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
| `BTC_IMAGE` | `bitcoin/bitcoin:29.0` | Docker image used by the 3 nodes. Default is pulled from the registry (no build needed). Set `simchainbitcoinnode:29.0` to use the locally built image (`./build-bitcoin.sh`). |
| `BITCOIN_VERSION` | `29.0` | Bitcoin Core version downloaded by `./build-bitcoin.sh` when building the local image. Not used by compose. |

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

## ZMQ notifications (node1 and node2)

node1 and node2 publish all five bitcoind ZMQ topics (`rawblock`, `rawtx`,
`hashblock`, `hashtx`, `sequence`), so ZMQ consumers (LND/CLN Lightning nodes,
ordinals indexers, block explorers, custody watchers) can run against the simnet,
including through reorgs. The variables below only change the **host** port
mappings; inside the compose network both nodes always publish on 28332-28336.

| Variable | Default | Description |
|---|---|---|
| `NODE1_ZMQ_RAWBLOCK_PORT` | `28332` | Host port for node1 `rawblock` (full serialized block). |
| `NODE1_ZMQ_RAWTX_PORT` | `28333` | Host port for node1 `rawtx` (full serialized tx). |
| `NODE1_ZMQ_HASHBLOCK_PORT` | `28334` | Host port for node1 `hashblock`. |
| `NODE1_ZMQ_HASHTX_PORT` | `28335` | Host port for node1 `hashtx`. |
| `NODE1_ZMQ_SEQUENCE_PORT` | `28336` | Host port for node1 `sequence` (mempool add/remove + block connect/disconnect, the reorg-aware topic). |
| `NODE2_ZMQ_RAWBLOCK_PORT` | `38332` | Host port for node2 `rawblock`. |
| `NODE2_ZMQ_RAWTX_PORT` | `38333` | Host port for node2 `rawtx`. |
| `NODE2_ZMQ_HASHBLOCK_PORT` | `38334` | Host port for node2 `hashblock`. |
| `NODE2_ZMQ_HASHTX_PORT` | `38335` | Host port for node2 `hashtx`. |
| `NODE2_ZMQ_SEQUENCE_PORT` | `38336` | Host port for node2 `sequence`. |

Smoke test: see the ZMQ section in the README.

## Container-internal RPC endpoints

URLs the helper tools (mining controller, spammer) use to reach the nodes inside the
compose network. Only change them if you rename the node services or point the tools
at other nodes.

| Variable | Default | Description |
|---|---|---|
| `NODE1_RPC_URL` | `http://btc-simnet-node1:18443` | Node1 RPC endpoint (the spammer watches it for new blocks). |
| `NODE2_RPC_URL` | `http://btc-simnet-node2:18443` | Node2 RPC endpoint (mining controller and spammer). |
| `NODE3_RPC_URL` | `http://btc-simnet-node3:18443` | Node3 RPC endpoint (mining controller and spammer). |

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
| `SPAM_TXS_PER_BLOCK` | `100` | Total spam txs offered per block — the number a block explorer shows per block (plus coinbase) as long as blocks are not already full; excess waits in the mempool. The spammer splits it across the miner wallets (currently node2 and node3) — how many miners exist is its responsibility, not the user's. Replaces the deprecated `SPAM_PER_MINER_PER_BLOCK` (still honored standalone: its value × 2). |
| `SPAM_SENDMANY_OUTPUTS` | `0` | `0`: sequential mode — one `sendtoaddress` RPC per tx, txs arrive at the mempool one by one like real p2p traffic. `N > 0`: batch mode — each spam tx is a single `sendmany` with N outputs (exchange-payout-shaped), so one RPC places N payments; needed to fill consensus-size blocks on short intervals (see [Full blocks](#full-blocks)). |
| `SPAM_FANOUT_UTXOS` | `50` | The spammer keeps each wallet split into this many independent UTXOs, replenishing when the pool runs low (startup, or after a reorg un-confirms the wallet's change). The mempool caps unconfirmed chains at 25 txs, so without the split a wallet can never place more than 25 txs per block. `0` disables. |
| `ENABLE_SPAM_REPLACES` | `false` | `true` or `1`: every spam tx signals RBF (BIP125) and, right after each batch, the newest `SPAM_REPLACES_PER_MINER_PER_BLOCK` txs per miner are fee-bumped with `bumpfee`, so the mempool carries real replacements (old txid evicted, new txid appears) for downstream code to handle. `false`/`0`: exactly today's behavior. |
| `SPAM_REPLACES_PER_MINER_PER_BLOCK` | `5` | How many of each miner's spam txs are fee-bumped per block when `ENABLE_SPAM_REPLACES` is on. The newest txs are bumped (a tx with unconfirmed descendants cannot be replaced). |

### Full blocks

The nodes keep Bitcoin Core's consensus-default block weight (4M WU, ~7,100 small
spam txs), so filling blocks is purely a question of feeding the mempool fast enough.
A 1-in/2-out spam tx is ~561 WU; sequential sending is bound by RPC round-trips
(~22 accepted tx/s on a mid-range desktop). Two ready-made setups:

Fast full blocks, under 1 minute each (batch mode):

```bash
BLOCK_INTERVAL_SECS=60
SPAM_TXS_PER_BLOCK=360
SPAM_SENDMANY_OUTPUTS=100     # 360 batches x ~12.7k WU ≈ 4.6M WU offered > 4M cap
```

Measured on a mid-range desktop: blocks land at ~3.98M WU (99.7% of the cap) and the
send cycle takes ~40–55s, so the occasional block right after a UTXO re-split comes
out partial; use `BLOCK_INTERVAL_SECS=90` if every single block must be full.

The spam outputs pay burn addresses (no known key), not wallet addresses, and that
is what makes sustained full blocks possible: bitcoind's coin selection scans the
whole wallet on every send, so when the spam used to pay the other miner's wallet,
each full block grew that wallet by `SPAM_TXS_PER_BLOCK × SPAM_SENDMANY_OUTPUTS`
dust UTXOs (~18k) until the send cycle no longer fit any interval (measured: 54s
fresh → 15+ min after ~2h). Burned dust never enters a wallet; the miners only keep
their own change, so the cycle time stays flat. The cost is a slow drain, ~0.16 BTC
per full block against a ~2550 BTC bootstrap balance — thousands of blocks of margin.
The spammer works both wallets in parallel (one thread per miner node), so the
cycle is bound by the slower half, not the sum. If blocks still come out partial,
check the real cycle time in `docker logs btc-simnet-spammer` (the
`Spam cycle done in ...` line each round) and keep `BLOCK_INTERVAL_SECS` above it.

Sequential p2p-like arrival (`SPAM_SENDMANY_OUTPUTS=0`), full blocks:

```bash
BLOCK_INTERVAL_SECS=420       # ~330s minimum at 22 tx/s; 420s reserves for slower machines
SPAM_TXS_PER_BLOCK=8000
SPAM_FANOUT_UTXOS=200         # 4000 txs per wallet need >= 160 independent 25-tx chains
```

With shorter sequential intervals blocks fill proportionally
(`fill ≈ interval × send_rate / 7100`), so expect ~5.5–7 minutes per full block
depending on machine speed.

## Reorg simulator (profile `reorg`)

| Variable | Default | Description |
|---|---|---|
| `REORG_DEPTH` | `3` | How many blocks to orphan per reorg. CLI argument overrides it: `./simulate-reorg.sh 5`. |
| _(CLI only)_ `empty` | off | Per-run argument, not an env var: `./simulate-reorg.sh 3 empty` mines empty replacement blocks (chaos reorg) and leaves the orphaned txs unconfirmed, instead of re-mining them. Chosen per run so real and empty reorgs can be interleaved on the same chain. |
| `REORG_MODE` | `once` | `once` = single reorg then exit. `auto` = reorg every `AUTO_REORG_EVERY_BLOCKS`. |
| `AUTO_REORG_EVERY_BLOCKS` | `20` | Auto mode cadence (x); must be greater than `REORG_DEPTH` (y). |
| `REORG_NODE` | `btc-simnet-node3` | Node used to fork the chain (a hidden miner is realistic). |
| `REORG_NODE_RPC_PORT` | `18443` | RPC port of `REORG_NODE` inside the compose network. |
| `REORG_MINE_ADDRESS` | `bcrt1qtmjq...tf3rr` | Address receiving the replacement block rewards. **The default is the same address as `USER_ADDRESS`'s default** (intentional), so after a reorg plus 100 blocks of maturity the user balance grows beyond the bootstrap 2x50 BTC. Set a separate throwaway address if your test asserts exact user balances. |
| `REORG_ADDS_NEW_TXS` | `5` | Fresh wallet txs seeded into the reorg node's mempool before mining, modelling a node that received transactions its peers have not yet seen; they are mined into the winning chain alongside the returned txs. `0` disables. Ignored for `empty` reorgs. To match spammed block fullness, set it near `SPAM_TXS_PER_BLOCK`. |
| `REORG_WALLET_NAME` | `NODE3_WALLET_NAME` (`node3`) | Wallet used to send the `REORG_ADDS_NEW_TXS` transactions on the reorg node. Falls back to the first loaded wallet if it is not loaded. |
| `REORG_WITNESS_NODE` | `btc-simnet-node1` | Node polled after mining the replacements to confirm the whole network adopted the new chain. If the mining controller extended the old chain during the reorg window (tie), extra blocks are mined (up to 10) until the witness follows the new tip. `none` disables the check. |

Orphaned transactions return to the mempool automatically. The replacement blocks are
filled by re-reading the mempool live and mining slices of it with `generateblock`, like
the winning chain of a real reorg, so the replacements are normally as full as the blocks
they replace. Reading the mempool fresh for each block means an RBF replacement that
evicts an orphaned tx mid-reorg (e.g. with `ENABLE_SPAM_REPLACES=true`) is picked up
automatically instead of leaving the block referencing a stale txid — no single rejection
can cascade the rest of the run to empty blocks.

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
