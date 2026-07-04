
FIX

btc-simnet-mining-controller  | 2026-07-04T22:02:11.032362415Z called `Result::unwrap()` on an `Err` value: JsonRpc(Rpc(RpcError { code: -4, message: "Wallet file verification failed. Failed to create database path '/home/bitcoin/.bitcoin/regtest/wallets/node2'. Database already exists.", data: None }))


## Limitations and future enhancements

### BitcoinCore containers
- Update the Bitcoin Core version: bump both the registry image default
  (`BTC_IMAGE=bitcoin/bitcoin:29.0` in compose/docs) and the local build default
  (`BITCOIN_VERSION=29.0` in `build-bitcoin.sh` / `.env`) to the latest release.
  When bumping, also review the Rust tools' `bitcoincore-rpc` crate: the tools use
  the `bitcoin` crate it re-exports, so upgrading `bitcoincore-rpc` is the only dep
  change needed, and each `bitcoincore-rpc` release documents the newest Core
  version it is tested against. Re-test bootstrap, spam and reorg flows after.
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
- Retries/idempotency for RPC calls (findings 1 and 2 below)

### Simulations
- Per-node policies: give each node different bitcoind parameters (mempool size,
  relay fees, RBF policy) or even different bitcoind versions/images, like a real
  heterogeneous network (the compose file already declares each node in full to
  allow this)
- Fee-market pressure: raise fees by spamming more/bigger transactions, and also via
  node policy parameters (`-minrelaytxfee` for the mempool floor, `-blockmintxfee`
  for the miner's inclusion floor) so blocks fill up and transactions genuinely
  compete for block space
- The six proposed features below: ZMQ, Poisson block timing, fee-market simulation,
  scenario engine, network partitions, reorgs that drop transactions

### Code review findings (2026-07-04)

Open findings from the last full code review, kept here so this is the single tracking
document. Everything the review found fixed has been dropped; the items below were
re-verified against the code on the review date.

Accepted decisions (not defects, recorded so they are not re-reported):

- **RPC bound on all host interfaces** (`-rpcallowip=0.0.0.0/0` + unrestricted port
  binding): intentional, reaching the simnet from another machine is a wanted use case.
- **Plaintext RPC credentials on the bitcoind command line** (visible in
  `docker inspect`/`ps`): acceptable for a throwaway regtest; documented with a warning
  in SETTINGS.md not to replicate in production.

Findings, ordered by severity:

1. **No RPC retries; transient errors are panics** (controller and spammer). Every call
   is `.unwrap()`: a node hiccup mid-run kills the process permanently, and neither
   service has a compose `restart` policy, so mining or spam silently stops. The reorg
   tool is the exception (Results, retry loop, long RPC timeout) and can serve as the
   template. Note the coupling with finding 2: adding `restart: on-failure` to the
   controller alone would produce a crash loop.

2. **Controller bootstrap is not idempotent.** `setup_wallet` calls
   `create_wallet(...).unwrap()` (`mining-controller/src/main.rs:39`); if the container
   restarts after the wallets exist it panics forever. Try `load_wallet`/`listwallets`
   first, or ignore the "already exists" error.

3. **Rust tool images are single-stage `rust:latest`** (~2.6 GB each; the Dockerfiles'
   own TODO). Multistage build, copy the binary into a slim runtime. Also listed under
   "Rust containers" above.

4. **Binary authenticity is not verified** in the bitcoin node image: SHA256SUMS comes
   from the same server as the tarball, GPG verification is commented out. Integrity
   yes, authenticity no. Also listed under "BitcoinCore containers" above.

5. **Dockerfile cleanup**: `RUN echo "UID/GID"` debug layers still present
   (Dockerfile:7-8), several mergeable `RUN` steps, duplicate `bitcoind -version`
   layers (120 and 122). Also listed under "BitcoinCore containers" above.

6. **Spammer↔controller implicit wallet contract (minor).** The spammer depends on
   wallets the controller creates, but `wait_for_funds` polls `get_balances` and
   tolerates a missing wallet indefinitely, so the contract no longer crashes anything;
   it just waits forever (one log line) if funding logic changes. Acceptable; a
   periodic "still waiting" log would make a misconfiguration visible.

7. **The forced reorg can lose the race against the mining controller.** The reorg tool
   mines exactly `depth+1` replacement blocks, one more than it invalidated, while the
   controller keeps mining the old chain on the other node every
   `BLOCK_INTERVAL_SECS`. If the controller lands a block during the reorg window, the
   old chain ties or stays ahead and the rest of the network never reorgs, while the
   tool still prints a success-looking summary (the replaced-blocks diff would be
   empty). Options: after mining the replacements, poll another node until it adopts
   the new tip and mine extra blocks until it does; or document that reliable reorgs
   need the controller stopped (or a `BLOCK_INTERVAL_SECS` comfortably larger than the
   reorg duration).

8. **No `Cargo.lock` is committed for any of the three tools** (all three
   `.gitignore`s exclude it; `git ls-files` confirms none tracked). Each fresh clone or
   image rebuild resolves dependencies anew, so two builds of the same commit can ship
   different dependency versions, the opposite of what a reproducible test network
   wants. Lockfiles should be committed for binary crates: drop `Cargo.lock` from the
   three `.gitignore`s and commit the locks.

9. **No root `.dockerignore` for the bitcoin node image build.** `build-bitcoin.sh`
   uses the repo root as build context; the context upload includes `.git` and the
   three Rust `target/` directories (easily GBs after local builds). The tool contexts
   have `.dockerignore`s; the root does not. Add one with at least `.git`, `*/target/`.

10. **`generateblock` fallback vacuums the mempool** (`reorg/src/main.rs:185-188`). If
    one tx-slice is rejected, the code falls back to `generate_to_address`, which mines
    the *entire* mempool into that block; every later chunk then references
    already-mined txids, so subsequent `generateblock` calls fail too and the remaining
    replacements degrade to empty/inject-only blocks. Logged, but surprising: after a
    fallback, later iterations could skip their (now stale) chunks deliberately.

11. **Bitcoin node base image is `debian:bullseye-slim`** (oldstable; security support
    ends mid-2026). The official `bitcoin/bitcoin` images are bookworm-based. Bump to
    `bookworm-slim` next time the image is touched.

12. **`btc-simnet-reorg` gates on the wrong node when `REORG_NODE` is overridden.**
    compose hardcodes `depends_on: btc-simnet-node3: service_healthy` while the target
    node is configurable; with `REORG_NODE=btc-simnet-node2` the one-shot run waits on
    node3's health instead. Harmless today (the tool also polls its node's RPC), worth
    a comment or depending on all nodes.

13. **Inconsistent required-vs-default env handling across the tools.** The controller
    `expect`s `USER_ADDRESS`/`BLOCK_INTERVAL_SECS`/credentials, the spammer `expect`s
    `SPAM_PER_MINER_PER_BLOCK`/credentials, yet sibling settings in the same files use
    `env_or` defaults, and the reorg tool defaults everything. Invisible under compose
    (it always passes them), but running a tool standalone panics on some vars and
    silently defaults others. Pick one policy (defaults everywhere matches the
    "runs with no .env" philosophy).

14. **No Cargo workspace; helpers duplicated three times.** `env_or`/`create_client`
    are copy-pasted per tool, and compose builds three independent dependency graphs
    serially (three `target/` dirs, three lock states). A workspace with one shared
    util crate, or a single multi-binary crate with three Dockerfile targets, would cut
    build time and future drift.

15. **`REORG_MINE_ADDRESS` default equals `USER_ADDRESS` default**, so replacement-block
    coinbases credit the user address: after a reorg (plus maturity) the "exactly
    2×50 BTC" user balance quietly grows. Fine if intended; either document it in
    SETTINGS.md or default to a separate throwaway address.

16. Nit: `inject_transactions` uses `wallets[0]` (first loaded wallet) on the reorg
    node instead of the known `NODE3_WALLET_NAME`; arbitrary if extra wallets are
    loaded on that node.

---

# Simchain Nice-to-have Features

Simchain's purpose is to simulate the Bitcoin chain on regtest while staying as close to
mainnet reality as regtest allows: multiple P2P-connected nodes, rotating miners, a
non-mining full node as the user endpoint, non-empty blocks, and user-controlled
parameters (block time, tx per block, reorgs, ...). This document gathers all the known
limitations and future enhancements, plus six bigger proposed features with their
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

## 6. Reorgs that drop transactions (confirmation-loss testing)

**What:** Two related additions to the reorg simulator:

1. **`REORG_REMINE_ORPHANED=false`**: mine the replacement blocks *without* the
   orphaned transactions (empty or inject-only blocks), leaving them in the mempool.
2. **Automated double-spend of orphaned spam txs**: for a configurable fraction of the
   orphaned *wallet-owned* (spam) transactions, include a conflicting transaction
   (same inputs, different output) in the replacement blocks so the originals become
   permanently invalid and can never re-confirm.

**Why it's a nice-to-have (the use case):** Today the simulator re-mines the orphaned
transactions into the replacement blocks (same txids), so a reorg only changes block
hashes/heights — a user's transaction never *loses* confirmations. But the scariest
real-world reorg scenario is exactly the opposite, and it is what exchanges, custody
watchers, indexers and payment processors need to test: *"my deposit had N
confirmations, a reorg happened, and now my transaction is not in the chain anymore."*
Downstream code must notice the confirmation count dropping back to 0 (or the tx
conflicting entirely) and un-credit / re-queue / alert accordingly. Neither case can be
produced by simchain today.

The two additions map to the two real outcomes:

- **Temporary drop (addition 1):** the excluded transactions fall back to the mempool
  and re-confirm in a later block, confirmed → 0-conf → confirmed again. This tests
  "did my code notice the confirmation count drop below its threshold?" (Stop the
  mining controller after the reorg to keep them unconfirmed indefinitely.)
- **Permanent drop (addition 2):** a double-spend in the winning chain kills the
  original transaction forever — the classic double-spend attack an exchange fears.
  This can only be automated for wallet-owned spam transactions: the user's own
  transactions are signed with external keys the reorg node does not hold, so a user
  wanting *their* tx permanently dropped must broadcast the conflicting tx themselves
  (RBF replacement of their own tx after running addition 1).

**Implementation plan:**
1. In `reorg/src/main.rs::do_reorg`, when `REORG_REMINE_ORPHANED=false`, skip the
   returned-tx chunking and mine every replacement block with `generateblock` and an
   explicit transaction list (empty, or only the injected txids). `generateblock` must
   be used for *all* replacement blocks in this mode: `generatetoaddress` would vacuum
   the mempool — orphaned txs included — right back into the new chain.
2. Wire `REORG_REMINE_ORPHANED` (default `true`, current behavior) through
   docker-compose.yml, `.env.full.example` and SETTINGS.md like the other settings.
3. For the double-spend mode: pick orphaned txs that spend the reorg node's wallet
   UTXOs, build conflicting raw txs (`createrawtransaction` on the same inputs to a
   fresh wallet address, `signrawtransactionwithwallet`), and pass them to
   `generateblock` in the replacement blocks. Setting sketch:
   `REORG_DOUBLE_SPEND_PCT=0..100` (default 0).
4. Log which txids were excluded/conflicted so tests can assert on them.

Effort: addition 1 small (a flag and a branch in existing logic); addition 2 medium
(raw-tx construction, only meaningful with spam enabled).

---

## Honorable mentions

- **Chain snapshot/restore**, named volumes + tar helper to save a chain state and rerun
  tests from it without re-mining 102 blocks each time.
- **RBF/CPFP traffic in the spammer**, a fraction of spam txs opt into RBF and later get
  fee-bumped, exercising replacement handling downstream.
