# Simchain Settings Reference

Every setting is read from `.env` by `docker-compose.yml`, and **every one has a
default**, so a missing variable (or no `.env` file at all) still works.- `.env.example`, short template with the most used settings.
- `.env.full.example`, complete template with everything below.

```bash
cp .env.example .env        # everyday version
cp .env.full.example .env   # everything tweakable
```

## Bitcoin node image

| Variable | Default | Description |
|---|---|---|
| `BTC_IMAGE` | `bitcoin/bitcoin:31.1` | Docker image used by the 3 nodes. Default is pulled from the registry (no build needed). Set `simchainbitcoinnode:31.1` to use the locally built image (`./build-bitcoin.sh`). |
| `BITCOIN_VERSION` | `31.1` | Bitcoin Core version downloaded by `./build-bitcoin.sh` when building the local image. Not used by compose. |

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
| `MIN_RELAY_TX_FEE` | `0.00001` | Node mempool/relay floor (feerate, BTC/kvB). Keep at the mainnet default; see [The fee market](#the-fee-market-what-spam-pays-and-how-to-set-a-price-floor) for why it is the wrong knob for a fee floor. |
| `FALLBACK_FEE` | `0.0001` | Wallet feerate when estimation has no data (BTC/kvB). Also the simnet's whole price level: all spam pays this rate, so raising it sets an economic fee floor — see [The fee market](#the-fee-market-what-spam-pays-and-how-to-set-a-price-floor). |
| `MAX_TX_FEE` | `10000000` | Wallet cap on the total fee of one tx (whole BTC). |
| `NODE1_DISABLE_WALLET` | `1` | node1 has no wallet by default: it mimics a 3rd-party production endpoint with no hot wallet online, so the user manages keys externally and submits signed raw transactions. Set `0` to enable the wallet. |

### The fee market: what spam pays, and how to set a price floor

Both spam engines pay the same rate; they just reach it differently. The raw engine
(`USE_RAW_TX_SPAM=true`, the default) sets `FALLBACK_FEE` explicitly on every
transaction it builds — no estimator involved at all. The wallet engine never sets
a fee: every send lets the sending node's wallet choose, the wallet asks its own
fee estimator, the estimator has no data on a fresh chain, and the wallet falls
back to `FALLBACK_FEE`. With the defaults every spam tx pays ~10 sat/vB — the
uniform rate visible in the explorer.

Under the wallet engine the estimator never escapes that level either. Once it has
data, its only data is the spam itself, and all of it confirmed at the fallback
rate — so it recommends that same rate back and the spam keeps paying it.
`FALLBACK_FEE` is therefore not just a bootstrap value: it sets the simnet's price
level permanently, whichever engine is active.

That makes it a one-line **economic fee floor**. Combine a
[full-blocks recipe](#full-blocks) with, say, `FALLBACK_FEE=0.001` (100 sat/vB) and
the background traffic outbids anything cheaper: a user transaction paying more than
the spam rate jumps the queue and confirms next block; one paying less still relays
fine (the relay floor stays at 1 sat/vB), sits visibly in the mempool, and full
blocks keep passing it over — exactly how mainnet feels in a high-fee period. The
floor only exists while spam keeps blocks full; with partial blocks everything
confirms and the floor vanishes.

The cost is mostly recycled, not burned: the spam fees end up in the blocks that
node2/node3 mine, so they return to the miner wallets as coinbase after the
100-block maturity. Under the wallet engine the wallets pay those fees directly;
under the raw engine they come out of the engine's own funds, which it pulled from
the miner wallets in the first place (and pulls again when its pool drains). Either
way, only the 546-sat burn outputs really leave the loop.

**When the floor leaks: packing granularity.** Block assembly walks the mempool by
descending feerate, and when the next spam tx does not fit the space left in the
block it keeps scanning down the ladder for anything that does. A tiny transaction
fits anywhere — so it rides the leftover gap into the next block even while paying
far below the floor. The gap is roughly one spam tx: ~20k WU with
`SPAM_SENDMANY_OUTPUTS=160`, ~127k WU at 1000, ~380k WU at 3000 (hundreds of small
txs slip through per block). The rule: **the floor holds for a transaction only if
the spam backlog contains transactions as small as it.** Sequential spam (~561 WU
per tx) makes the floor airtight; big batches are for throughput and mempool-bloat
demos and actively break floor testing. If you are testing a fee-bumping engine on
a small transaction, use the sequential recipe (or see the hybrid-spam proposal in
`nice-to-have.md`, which combines batch bulk with small gap-sealing txs).

**Do not use `MIN_RELAY_TX_FEE` as the floor.** The wallets would cope — Bitcoin
Core clamps every wallet send to `max(-mintxfee, -minrelaytxfee)`, so spam would
still relay — but the semantics are wrong twice. It is policy drift: mainnet's
relay floor is 1 sat/vB, and raising it makes the simnet's nodes stop behaving like
mainnet nodes. And it turns the floor into a hard reject: a cheap user transaction
bounces at node1 with `min relay fee not met` instead of waiting in the mempool
like it would on mainnet. Fee pressure should come from traffic (tooling), not from
node policy.

Limitation: all spam sits in one fee bucket at whatever level you set — fee
histograms stay flat and `estimatesmartfee` just echoes the level. A spread of fee
rates with real competition inside a block is a proposed feature (nice-to-have:
fee-market simulation).

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
| `USE_RAW_TX_SPAM` | `true` | Selects the spam engine. `true`: **raw engine** — the spammer holds its own keys, tracks its own UTXO set in memory, signs every tx locally and submits with `sendrawtransaction`. The node wallets are bypassed, so the send rate stays flat forever (no wallet fatigue) and every tx pays exactly `FALLBACK_FEE`. `false`: **node-wallet engine** — spam is sent with `sendtoaddress`/`sendmany` on the miner wallets, so bitcoind does coin selection and signing (wallet-realistic traffic, the original behavior); throughput is bound by the wallet lock and degrades as wallet history grows (see [Full blocks](#full-blocks)). All other spam knobs apply to both engines. |
| `SPAM_FIXED_TXS_PER_BLOCK` | `100` | Fixed tx count for the **OUTPUT** spam modes (sequential/batch) and the wallet engine — the number a block explorer shows per block (plus coinbase) until blocks are full; excess waits in the mempool. Split across the miner nodes for you. **Ignored in DATA/HYBRID mode**, where the fill is driven by `SPAM_FILL_BLOCK_RATIO`. Renamed from `SPAM_TXS_PER_BLOCK` (still honored); replaces the older `SPAM_PER_MINER_PER_BLOCK` (× 2). |
| `SPAM_SENDMANY_OUTPUTS` | `0` | OUTPUT-mode fatness. `0`: sequential — one tx with a single burn output at a time, p2p-like arrival. `N > 0`: batch — each spam tx carries N burn outputs (exchange-payout-shaped). Ignored in DATA/HYBRID mode. |
| `SPAM_TX_DATA_MAX_BYTES` | `0` | Raw engine only. `0`: OUTPUT mode (fatness from burn outputs). `N > 0`: **DATA/HYBRID mode** — the fill comes from OP_RETURN data txs (biggest payload = N). An OP_RETURN is provably unspendable, so it never enters the UTXO set: pure block weight at near-zero node cost (a handful of fat txs fill a 4M WU block vs ~1130 in output mode; measured node CPU ~100% → ~2%). Capped just under the 100k-vB standard-tx limit; needs Core 30+. Renamed from `SPAM_TX_DATA_BYTES` (still honored). See [Hybrid: varied sizes and mempool depth](#hybrid-varied-sizes-and-mempool-depth). |
| `SPAM_TX_DATA_MIN_BYTES` | `0` | Smallest data payload. `0` (or ≥ MAX): every data tx is exactly MAX (uniform). Below MAX: each tx's size is drawn **log-uniformly** in `[MIN, MAX]` — a realistic spread, most small and a few large. |
| `SPAM_SMALL_TXS_PER_BLOCK` | `0` | HYBRID: this many extra minimum-size (~140 vB) floor-priced txs per block, on top of the data fill. They tighten packing and add a stream of small realistic-looking txs. **The fee floor is not yet airtight** — see [The fee floor is soft](#the-fee-floor-is-soft). `0`: none. |
| `SPAM_FILL_BLOCK_RATIO` | `1.0` | DATA/HYBRID fill target, in blocks of mempool weight, measured live each block and topped up. `0.5`: half-full blocks (floor off). `1`: full blocks + a shallow backlog. `5`: full blocks + ~4 pending blocks visible in the mempool. |
| `SPAM_FANOUT_AUTO` | `true` | DATA/HYBRID: auto-size the branch pool from the fill ratio. `true`: use `max(12, ceil(ratio × 15))` branches (a deep pool is needed to hold that many blocks of unconfirmed spam). `false`: use `SPAM_FANOUT_UTXOS`, erroring at startup if it is below the `ratio × 10` minimum. |
| `SPAM_FANOUT_UTXOS` | `50` | The spammer keeps its funds split into this many independent UTXOs ("branches"), replenishing when the pool runs low. The mempool caps unconfirmed chains at 25 txs / 101k vB, so without the split a single UTXO can place only ~25 txs per block. In DATA/HYBRID mode this is overridden by the auto value unless `SPAM_FANOUT_AUTO=false`. `0` disables (OUTPUT/wallet only). |
| `ENABLE_SPAM_REPLACES` | `false` | `true` or `1`: every spam tx signals RBF (BIP125) and, right after each batch, the newest `SPAM_REPLACES_PER_MINER_PER_BLOCK` txs per miner are fee-bumped with `bumpfee`, so the mempool carries real replacements (old txid evicted, new txid appears) for downstream code to handle. `false`/`0`: exactly today's behavior. |
| `SPAM_REPLACES_PER_MINER_PER_BLOCK` | `5` | How many of each miner's spam txs are fee-bumped per block when `ENABLE_SPAM_REPLACES` is on. The newest txs are bumped (a tx with unconfirmed descendants cannot be replaced). |

### Full blocks

The nodes keep Bitcoin Core's consensus-default block weight (4M WU, ~7,100 small
spam txs), so filling blocks is purely a question of feeding the mempool fast enough.
A 1-in/2-out spam tx is ~561 WU; sequential sending is bound by RPC round-trips
(~22 accepted tx/s on a mid-range desktop). The measured numbers in this section
were taken with the wallet engine (`USE_RAW_TX_SPAM=false`); the raw engine
(default) is substantially faster at the same settings — check your real cycle time
in the `Spam cycle done in ...` log line. Two ready-made setups:

Fast full blocks, under 1 minute each (batch mode):

```bash
BLOCK_INTERVAL_SECS=60
SPAM_FIXED_TXS_PER_BLOCK=360
SPAM_SENDMANY_OUTPUTS=100     # 360 batches x ~12.7k WU ≈ 4.6M WU offered > 4M cap
```

Measured on a mid-range desktop: blocks land at ~3.98M WU (99.7% of the cap) and the
send cycle takes ~40–55s, so the occasional block right after a UTXO re-split comes
out partial; use `BLOCK_INTERVAL_SECS=90` if every single block must be full.

The spam outputs pay burn addresses (no known key), not wallet addresses, and that
is what makes sustained full blocks possible: bitcoind's coin selection scans the
whole wallet on every send, so when the spam used to pay the other miner's wallet,
each full block grew that wallet by `SPAM_FIXED_TXS_PER_BLOCK × SPAM_SENDMANY_OUTPUTS`
dust UTXOs (~18k) until the send cycle no longer fit any interval (measured: 54s
fresh → 15+ min after ~2h). Burned dust never enters a wallet; the miners only keep
their own change, so the cycle time stays flat. The cost is a slow drain, ~0.16 BTC
per full block against a ~2550 BTC bootstrap balance — thousands of blocks of margin.
The spammer works both wallets in parallel (one thread per miner node), so the
cycle is bound by the slower half, not the sum. If blocks still come out partial,
check the real cycle time in `docker logs btc-simnet-spammer` (the
`Spam cycle done in ...` line each round) and keep `BLOCK_INTERVAL_SECS` above it.
If the cycle time *grows* over the session instead, that is wallet fatigue:
bitcoind keeps the whole wallet tx history in memory and scans it on every send
(measured: ~13s cycle fresh → ~67s after ~50 full blocks). It is inherent to
wallet-based spam, i.e. to `USE_RAW_TX_SPAM=false`; the raw engine (default) is
immune — its bookkeeping is a constant-size in-memory UTXO set, so switching back
to it is the structural fix. Wallet-engine resets: a stack restart
(`docker compose down -v`) or lowering the offered tx count.

Sequential p2p-like arrival (`SPAM_SENDMANY_OUTPUTS=0`), full blocks:

```bash
BLOCK_INTERVAL_SECS=420       # ~330s minimum at 22 tx/s; 420s reserves for slower machines
SPAM_FIXED_TXS_PER_BLOCK=8000
SPAM_FANOUT_UTXOS=200         # 4000 txs per wallet need >= 160 independent 25-tx chains
```

With shorter sequential intervals blocks fill proportionally
(`fill ≈ interval × send_rate / 7100`), so expect ~5.5–7 minutes per full block
depending on machine speed.

Data mode makes blocks heavy with *data* instead of *transactions* — the thing that
loads a node (signature checks, mempool package math, and, in output/batch mode, a
UTXO-set insert per output: measured ~31k new UTXOs per full block). An OP_RETURN
output is provably unspendable, so it never enters the UTXO set. About 11 max-size
(90k-byte) txs fill a 4M WU block versus ~1130 in batch mode, so the per-tx work
collapses: on the same machine that sat pegged at ~100% node CPU under batch spam,
data mode runs at **~2% node CPU**, blocks still ~99% full, the send cycle under a
second. It is the way to run *fast* full blocks without the nodes — or your machine —
becoming the limit. The recommended way to use it is **HYBRID mode** below (a spread of
sizes + a few small txs), not a single fixed size.

### Hybrid: varied sizes and mempool depth

HYBRID mode fills blocks with data txs of *varied* sizes (a realistic mempool look)
plus a few small txs, and keeps the mempool a chosen number of blocks deep:

```bash
SPAM_TX_DATA_MAX_BYTES=90000   # biggest OP_RETURN payload (cheap bulk weight)
SPAM_TX_DATA_MIN_BYTES=250     # spread each tx's size log-uniformly in [MIN, MAX]
SPAM_SMALL_TXS_PER_BLOCK=40    # small floor-priced txs (soft floor — see below)
SPAM_FILL_BLOCK_RATIO=2        # keep ~2 blocks of weight pending in the mempool
FALLBACK_FEE=0.001             # 100 sat/vB price level for all spam
```

`SPAM_FILL_BLOCK_RATIO` is measured live each block and topped up, so it controls both
fullness *and* mempool depth from one dial: `0.5` → half-full blocks (an uncongested
chain), `1` → full blocks with a shallow backlog, `5` → full blocks with ~4 pending
blocks visible in mempool.space. The branch pool auto-sizes to the ratio
(`SPAM_FANOUT_AUTO=true` → `max(12, ratio × 15)` branches); a deep pool is needed
because the mempool caps each unconfirmed chain at ~101k vB, so holding `R` blocks of
unconfirmed spam needs about `R × 10` branches. Sizes verified live spanning ~141 vB
(smallest) to ~88k vB in a single mempool; node CPU stays low and the UTXO set barely
grows (only the small txs add outputs).

### The fee floor is soft

Raising `FALLBACK_FEE` with full blocks makes the estimator and mempool.space show a
price floor (e.g. Low/Medium/High all at 100 sat/vB). **But the floor is not airtight
against tiny transactions.** Blocks pack to ~98–99%, not 100%, and a cheap tiny tx
(below the floor rate) can still slip into the leftover ~12–17k-vB packing gap and
confirm next block — visible as the `No Priority` band sitting at ~1–2 sat/vB.

Why: every spam tx chains off a branch, so only a branch-count of them are *standalone*
(mineable into a small gap on their own); the rest are chain tips whose ancestor
package is far too big to fit a gap. And any floor-priced tx the engine does make gets
*mined* (it pays the floor), so none persist to guard the residual gap. The
`SPAM_SMALL_TXS_PER_BLOCK` gap-sealers tighten packing and raise the bar — the floor
holds for any tx *larger* than the gap — but they do not close it for the smallest txs.
Closing it fully needs a pool of standalone confirmed UTXOs feeding the current block's
fill (planned — see `nice-to-have.md`). For a genuinely airtight floor today, use the
small-tx [Market pressure](#market-pressure-floor-to-100-satsvb) recipe below (higher
node load), whose spam is itself small enough to leave no exploitable gap.

### Market pressure floor to 100 sats/vB
full blocks with 100 sats/vb every tx
```bash
BLOCK_INTERVAL_SECS=15
ENABLE_SPAM=true
SPAM_FIXED_TXS_PER_BLOCK=250
SPAM_SENDMANY_OUTPUTS=250
FALLBACK_FEE=0.001

```

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
| `REORG_ADDS_NEW_TXS` | `5` | Fresh wallet txs seeded into the reorg node's mempool before mining, modelling a node that received transactions its peers have not yet seen; they are mined into the winning chain alongside the returned txs. `0` disables. Ignored for `empty` reorgs. To match spammed block fullness, set it near `SPAM_FIXED_TXS_PER_BLOCK` (OUTPUT mode). |
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

# Mempool space picture with market pressure floor at 1000 sats/vB

![Red chain diagram](img/red-chain.png)
