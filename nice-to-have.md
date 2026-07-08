## Limitations and future enhancements

### BitcoinCore containers
- Update the Bitcoin Core version: bump both the registry image default
  (`BTC_IMAGE=bitcoin/bitcoin:29.0` in compose/docs) and the local build default
  (`BITCOIN_VERSION=29.0` in `build-bitcoin.sh` / `.env`) to the latest release.
  When bumping, also review the Rust tools' `bitcoincore-rpc` crate: the tools use
  the `bitcoin` crate it re-exports, so upgrading `bitcoincore-rpc` is the only dep
  change needed, and each `bitcoincore-rpc` release documents the newest Core
  version it is tested against. Re-test bootstrap, spam and reorg flows after.
- Build from sources instead of downloading binaries
- Clean up the Dockerfile: drop the debug `RUN echo UID/GID` layers and merge related
  `RUN` steps to reduce image layers

### Rust containers
- Use multistage builds in the Dockerfiles: build with `rust:latest`, copy only the
  binary into a slim runtime image (each tool image is currently ~2.6GB)
- Retries for RPC calls (finding 1 below)

### Simulations
- Per-node policies: give each node different bitcoind parameters (mempool size,
  relay fees, RBF policy) or even different bitcoind versions/images, like a real
  heterogeneous network (the compose file already declares each node in full to
  allow this)
- Fee-market pressure: raise fees by spamming more/bigger transactions, and also via
  node policy parameters (`-minrelaytxfee` for the mempool floor, `-blockmintxfee`
  for the miner's inclusion floor) so blocks fill up and transactions genuinely
  compete for block space
- The five proposed features below: Poisson block timing, fee-market simulation,
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
   template. The controller bootstrap is now restart-safe (wallets are loaded if they
   exist, funding is skipped once the chain is bootstrapped), so adding
   `restart: on-failure` to both services is a cheap interim mitigation.

2. **Rust tool images are single-stage `rust:latest`** (~2.6 GB each; the Dockerfiles'
   own TODO). Multistage build, copy the binary into a slim runtime. Also listed under
   "Rust containers" above.

3. **Dockerfile cleanup**: `RUN echo "UID/GID"` debug layers still present, several
   mergeable `RUN` steps, duplicate `bitcoind -version` layers. Also listed under
   "BitcoinCore containers" above.

4. **Spammer↔controller implicit wallet contract (minor).** The spammer depends on
   wallets the controller creates, but `wait_for_funds` polls `get_balances` and
   tolerates a missing wallet indefinitely, so the contract no longer crashes anything;
   it just waits forever (one log line) if funding logic changes. Acceptable; a
   periodic "still waiting" log would make a misconfiguration visible.

5. **No `Cargo.lock` is committed for any of the three tools** (all three
   `.gitignore`s exclude it; `git ls-files` confirms none tracked). Each fresh clone or
   image rebuild resolves dependencies anew, so two builds of the same commit can ship
   different dependency versions, the opposite of what a reproducible test network
   wants. Lockfiles should be committed for binary crates: drop `Cargo.lock` from the
   three `.gitignore`s and commit the locks.

6. **Bitcoin node base image is `debian:bullseye-slim`** (oldstable; security support
   ends mid-2026). The official `bitcoin/bitcoin` images are bookworm-based. Bump to
   `bookworm-slim` next time the image is touched.

7. **No Cargo workspace; helpers duplicated three times.** `env_or`/`create_client`
   are copy-pasted per tool, and compose builds three independent dependency graphs
   serially (three `target/` dirs, three lock states). A workspace with one shared
   util crate, or a single multi-binary crate with three Dockerfile targets, would cut
   build time and future drift.

---

# Simchain Nice-to-have Features

Simchain's purpose is to simulate the Bitcoin chain on regtest while staying as close to
mainnet reality as regtest allows: multiple P2P-connected nodes, rotating miners, a
non-mining full node as the user endpoint, non-empty blocks, and user-controlled
parameters (block time, tx per block, reorgs, ...). This document gathers all the known
limitations and future enhancements, plus five bigger proposed features with their
rationale and an implementation plan.

## 1. Realistic block timing and hashrate distribution

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

## 2. Fee-market simulation in the spammer

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

Effort: medium. Pairs well with feature 1 (bursty blocks + fee spread = realistic mempool).

---

## 3. Declarative scenario engine

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

## 4. Network partition / latency simulation

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
3. Expose as compose profile `partition` and/or a scenario-engine action (feature 3),
   with settings `PARTITION_NODE`, `PARTITION_BLOCKS`.

Effort: phase 1 small; phase 2 medium (needs NET_ADMIN and per-node sidecars).

---

## 5. Reorgs that drop transactions (confirmation-loss testing)

**What:** Two related additions to the reorg simulator:

1. ~~**Mine the replacement blocks *without* the orphaned transactions**~~ **(DONE):**
   the `empty` CLI argument (`./simulate-reorg.sh <depth> empty`) mines empty
   replacement blocks and leaves the orphaned txs in the mempool — the temporary-drop
   scenario below. Chosen per run rather than via an env var so real and empty reorgs
   can be interleaved on the same chain.
2. **Automated double-spend of orphaned spam txs** (still open): for a configurable
   fraction of the orphaned *wallet-owned* (spam) transactions, include a conflicting
   transaction (same inputs, different output) in the replacement blocks so the
   originals become permanently invalid and can never re-confirm.

**Why it's a nice-to-have (the use case):** Today the simulator re-mines the orphaned
transactions into the replacement blocks (same txids), so a reorg only changes block
hashes/heights — a user's transaction never *loses* confirmations. But the scariest
real-world reorg scenario is exactly the opposite, and it is what exchanges, custody
watchers, indexers and payment processors need to test: *"my deposit had N
confirmations, a reorg happened, and now my transaction is not in the chain anymore."*
Downstream code must notice the confirmation count dropping back to 0 (or the tx
conflicting entirely) and un-credit / re-queue / alert accordingly. The temporary-drop
case now exists (addition 1, the `empty` mode); the permanent-drop case (addition 2)
cannot be produced by simchain today.

The two additions map to the two real outcomes:

- **Temporary drop (addition 1, done):** the excluded transactions fall back to the
  mempool and re-confirm in a later block, confirmed → 0-conf → confirmed again. This
  tests "did my code notice the confirmation count drop below its threshold?" (Stop the
  mining controller after the `empty` reorg to keep them unconfirmed indefinitely.)
- **Permanent drop (addition 2):** a double-spend in the winning chain kills the
  original transaction forever — the classic double-spend attack an exchange fears.
  This can only be automated for wallet-owned spam transactions: the user's own
  transactions are signed with external keys the reorg node does not hold, so a user
  wanting *their* tx permanently dropped must broadcast the conflicting tx themselves
  (RBF replacement of their own tx after running addition 1).

**Implementation plan:**
1. ~~Skip re-mining and mine empty replacement blocks with `generateblock`.~~ **Done:**
   `do_reorg`'s `empty` branch mines every replacement block (and any race-winning extra
   block) with `generateblock`, never `generatetoaddress`, so the orphaned txs are not
   vacuumed back into the new chain.
2. For the double-spend mode: pick orphaned txs that spend the reorg node's wallet
   UTXOs, build conflicting raw txs (`createrawtransaction` on the same inputs to a
   fresh wallet address, `signrawtransactionwithwallet`), and pass them to
   `generateblock` in the replacement blocks. Setting sketch:
   `REORG_DOUBLE_SPEND_PCT=0..100` (default 0).
3. Log which txids were conflicted so tests can assert on them.

Effort: addition 1 done; addition 2 medium (raw-tx construction, only meaningful with
spam enabled).


---

## Tech debt

- use a rust logging tool instead of print
- use a rust error managing tool

---

## Honorable mentions

- **Chain snapshot/restore**, named volumes + tar helper to save a chain state and rerun
  tests from it without re-mining 102 blocks each time.

- **CPFP traffic in the spammer**, some spam txs paying a too-low fee and a child
  bumping them (RBF replacements are implemented: `ENABLE_SPAM_REPLACES`).
