## Limitations and future enhancements

### BitcoinCore containers
- Download PGP signatures and verify the downloaded binaries: the SHA256 checksum is
  verified, but SHA256SUMS comes from the same server as the tarball, so this proves
  integrity, not authenticity (the GPG key import block exists in the Dockerfile,
  commented out)
- Build from sources instead of downloading binaries
- Clean up the Dockerfile: drop the debug `RUN echo UID/GID` layers and merge related
  `RUN` steps to reduce image layers

### Rust containers
- Use multistage builds in the Dockerfiles: build with `rust:latest`, copy only the
  binary into a slim runtime image (each tool image is currently ~2.6GB)
- Retries/idempotency for RPC calls (see [review.md](./review.md))

### Simulations
- Per-node policies: give each node different bitcoind parameters (mempool size,
  relay fees, RBF policy) or even different bitcoind versions/images, like a real
  heterogeneous network (the compose file already declares each node in full to
  allow this)
- Fee-market pressure: raise fees by spamming more/bigger transactions, and also via
  node policy parameters (`-minrelaytxfee` for the mempool floor, `-blockmintxfee`
  for the miner's inclusion floor) so blocks fill up and transactions genuinely
  compete for block space
- The five proposed features below: ZMQ, Poisson block timing, fee-market simulation,
  scenario engine, network partitions

---

# Simchain Nice-to-have Features

Simchain's purpose is to simulate the Bitcoin chain on regtest while staying as close to
mainnet reality as regtest allows: multiple P2P-connected nodes, rotating miners, a
non-mining full node as the user endpoint, non-empty blocks, and user-controlled
parameters (block time, tx per block, reorgs, ...). This document gathers all the known
limitations and future enhancements, plus five bigger proposed features with their
rationale and an implementation plan.

## 1. ZMQ notifications on node1 and node2

**What:** Enable bitcoind's ZeroMQ publishers (`rawblock`, `rawtx`, `hashblock`,
`hashtx`, `sequence`) on the user-facing nodes and expose the ports to the host.

**Why it's a nice-to-have:** Almost every serious project built on top of Bitcoin
(LND/CLN Lightning nodes, ordinals indexers, block explorers, custody watchers) consumes
ZMQ instead of polling RPC. Today those stacks cannot be tested against simchain (the
README even warns node2 has no ZMQ). Adding it makes simchain a drop-in backend for the
exact class of projects it exists to serve, and ZMQ delivery during a reorg is precisely
the hard case people need to test.

**Implementation plan:**
1. Add to node1/node2 commands: `-zmqpubrawblock=tcp://0.0.0.0:28332`,
   `-zmqpubrawtx=tcp://0.0.0.0:28333`, `-zmqpubsequence=tcp://0.0.0.0:28334`.
2. Parameterize host mappings in `.env`: `NODE1_ZMQ_BLOCK_PORT=28332`, etc., with
   defaults in the compose file like every other setting.
3. Document a smoke test (`python -c` zmq subscriber or `lnd --bitcoin.node=bitcoind`
   pointing at node2) in the README.

Effort: small (compose + docs only). No code changes.

---

## 2. Realistic block timing and hashrate distribution

**What:** Replace the fixed `BLOCK_INTERVAL_SECS` + strict node2/node3 alternation with
(a) Poisson-distributed block intervals (exponential inter-arrival times with the
configured mean) and (b) weighted miner selection (e.g. `MINER_WEIGHTS=70,30`).

**Why it's a nice-to-have:** On mainnet, blocks are a Poisson process: two blocks 20
seconds apart followed by a 40-minute gap is normal, and that variance is what breaks
naive confirmation logic, fee estimators, and timeout handling in downstream projects. A
metronomic 15s cadence with perfect miner alternation hides an entire class of bugs.
Weighted hashrate also makes reorg/selfish-mining scenarios meaningful (a 70% miner
winning races is realistic; a coin-flip is not).

**Implementation plan:**
1. In `mining-controller`, add `rand`/`rand_distr` crates; sample the sleep from
   `Exp::new(1.0 / mean)` when `BLOCK_INTERVAL_MODE=poisson` (default stays `fixed`).
2. Pick the miner per block by sampling `MINER_WEIGHTS` (comma-separated, default `50,50`)
   instead of toggling.
3. Surface both as `.env` settings wired through compose with defaults; log the sampled
   interval and chosen miner each block so tests can correlate.

Effort: small-medium (contained in one Rust file).

---

## 3. Fee-market simulation in the spammer

**What:** Make the spammer emit transactions with varied fee rates (sampled from a
configurable distribution, e.g. log-normal between `SPAM_FEE_MIN`/`SPAM_FEE_MAX` sat/vB)
and varied sizes/output counts, instead of identical 540-sat dust sends at fallback fee.

**Why it's a nice-to-have:** With uniform transactions, `estimatesmartfee`, mempool fee
histograms (visible in the mempool explorer) and any RBF/fee-bumping logic in the project
under test are meaningless, everything sits in one fee bucket. A spread of fee rates
creates real block-space competition: when spam volume exceeds block capacity, low-fee
transactions genuinely wait, which is exactly the mainnet behavior users want to
reproduce with the "tx per block" knob.

**Implementation plan:**
1. In `spammer`, switch from `send_to_address` defaults to passing an explicit
   `fee_rate` (bitcoincore-rpc `send` / `sendtoaddress` fee_rate arg), sampled per tx.
2. Add `.env` settings with defaults: `SPAM_FEE_MIN=1`, `SPAM_FEE_MAX=50`,
   `SPAM_OUTPUTS_MAX=4` (multi-output txs via `send_many` for size variance).
3. Log a per-batch fee summary; verify the histogram in the mempool explorer.

Effort: medium. Pairs well with feature 2 (bursty blocks + fee spread = realistic mempool).

---

## 4. Declarative scenario engine

**What:** A `scenario.yml` interpreted by a small controller container: an ordered list of
steps like *"at height 150 reorg 2 blocks"*, *"pause mining 120s"*, *"burst 500 txs"*,
*"partition node3 for 3 blocks, then heal"*. A `scenario` compose profile runs it.

**Why it's a nice-to-have:** Today reproducing a test case means hand-running
`bitcoin-cli`/reorg commands in the right order at the right time. A scenario file makes
chain histories **reproducible and shareable**, a bug report can include the exact
scenario that triggers it, and downstream projects can pin scenarios in CI ("our indexer
must survive `reorg-during-sync.yml`"). This turns simchain from an environment into a
test harness.

**Implementation plan:**
1. Define a minimal step schema: `at: {height|time}`, `action:
   {mine, pause_mining, reorg, spam_burst, disconnect, connect}`, `params: {...}`.
2. Implement an interpreter (Rust to match the repo, or Python for speed of iteration)
   that polls node1 height and drives the existing pieces over RPC; reuse the
   `reorg` crate's logic for the reorg action.
3. Coordinate with the mining controller via a simple flag: either the scenario engine
   *replaces* it (`MINING_MODE=scenario`), or exposes pause/resume through a tiny control
   file/HTTP endpoint the controller checks each loop.
4. Ship 2–3 example scenarios in `scenarios/`; add compose service with
   `profiles: ["scenario"]` mounting the chosen file.

Effort: the largest item here, but mostly glue around already-existing capabilities.

---

## 5. Network partition / latency simulation

**What:** Tooling to split the P2P network (e.g. isolate node3, let it mine alone, then
reconnect) and to inject latency/packet loss between nodes, via `docker network
disconnect/connect` or `tc netem` in a helper container.

**Why it's a nice-to-have:** Real reorgs are *caused* by propagation delays and network
partitions; today's reorg simulator forces one administratively (`invalidateblock`).
A partition that heals produces organic competing chains, natural orphan races, and
double-spend windows, the scenarios exchanges and payment processors actually fear.
Latency injection also makes block/tx propagation observable (compare heights across
nodes during the window), which no instantaneous regtest network shows.

**Implementation plan:**
1. Phase 1 (no new images): `partition.sh` helper using `docker network disconnect
   btc-simnet-network btc-simnet-node3` + reconnect after N seconds/blocks; while split,
   direct mining on both sides via RPC so competing chains grow.
2. Phase 2: optional latency profile, run nodes with `cap_add: NET_ADMIN` and a sidecar
   applying `tc qdisc add dev eth0 root netem delay 500ms loss 1%`, parameterized via
   `.env` (`P2P_DELAY_MS`, `P2P_LOSS_PCT`).
3. Expose as compose profile `partition` and/or a scenario-engine action (feature 4),
   with settings `PARTITION_NODE`, `PARTITION_BLOCKS`.

Effort: phase 1 small; phase 2 medium (needs NET_ADMIN and per-node sidecars).

---

## Honorable mentions

- **Chain snapshot/restore**, named volumes + tar helper to save a chain state and rerun
  tests from it without re-mining 102 blocks each time.
- **RBF/CPFP traffic in the spammer**, a fraction of spam txs opt into RBF and later get
  fee-bumped, exercising replacement handling downstream.
- **Multi-wallet RPC paths**, wallet-scoped RPC URLs (`/wallet/<name>`) in controller and
  spammer so users can load extra wallets on node2 without breaking the tooling
  (see review.md #12/#15).
