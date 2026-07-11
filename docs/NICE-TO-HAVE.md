# Code review findings

Open findings from the last full code review, kept here so this is the single tracking
document. Everything the review found fixed has been dropped; the items below were
re-verified against the code on the review date.

Accepted decisions (not defects, recorded so they are not re-reported):

- **RPC bound on all host interfaces** (`-rpcallowip=0.0.0.0/0` + unrestricted port
  binding): intentional, reaching the simnet from another machine is a wanted use case.

- **Plaintext RPC credentials on the bitcoind command line** (visible in
  `docker inspect`/`ps`): acceptable for a throwaway regtest; documented with a warning
  in SETTINGS.md not to replicate in production.

No open findings from the last review remain.

---

# Limitations and future enhancements


## Simulations

- Per-node policies: give each node different bitcoind parameters (mempool size,
  relay fees, RBF policy) or even different bitcoind versions/images, like a real
  heterogeneous network (the compose file already declares each node in full to
  allow this)


# Simchain Nice-to-have Features

Simchain's purpose is to simulate the Bitcoin chain on regtest while staying as close to
mainnet reality as regtest allows, but also providing a "controlled by the user environment"
that allows to defining mining pace, block filling and fee rates.
It consists on: multiple P2P-connected nodes, rotating miners,
a non-mining full node as the user endpoint, non-empty blocks, and user-controlled
parameters (block time, tx per block, reorgs, ...). This document gathers all the known
limitations and future enhancements, plus five bigger proposed features with their
rationale and an implementation plan, and a section for parked features.

## 1. Declarative scenario engine

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

## 2. Network partition / latency simulation

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
3. Expose as compose profile `partition` and/or a scenario-engine action (feature 1),
   with settings `PARTITION_NODE`, `PARTITION_BLOCKS`.

Effort: phase 1 small; phase 2 medium (needs NET_ADMIN and per-node sidecars).

---

## 3. Reorgs that drop transactions permanently (double-spend)

**What:** For a configurable fraction of the orphaned *wallet-owned* (spam) transactions
in a reorg, include a conflicting transaction (same inputs, different output) in the
replacement blocks, so the originals become permanently invalid and can never
re-confirm. Setting sketch: `REORG_DOUBLE_SPEND_PCT=0..100` (default 0).

**Why it's a nice-to-have (the use case):** By default the reorg simulator re-mines the
orphaned transactions into the replacement blocks (same txids), so a reorg only changes
block hashes/heights — a user's transaction never *loses* confirmations. The
temporary-drop case (confirmed → 0-conf → re-confirmed) is already available through the
`empty` reorg mode (`./scripts/simulate-reorg.sh <depth> empty`, which mines empty replacement
blocks and leaves the orphaned txs in the mempool). But the scariest real reorg is
*permanent*: *"my deposit had N confirmations, a reorg happened, and now my transaction
is gone forever."* Exchanges, custody watchers, indexers and payment processors must
notice the tx conflicting entirely and un-credit / re-queue / alert — and simchain
cannot produce that outcome today.

This can only be automated for wallet-owned spam transactions: the user's own
transactions are signed with external keys the reorg node does not hold, so a user
wanting *their* tx permanently dropped must broadcast the conflicting tx themselves
(an RBF replacement after an `empty` reorg).

**Implementation plan:**
1. Pick orphaned txs that spend the reorg node's wallet UTXOs, build conflicting raw txs
   (`createrawtransaction` on the same inputs to a fresh wallet address,
   `signrawtransactionwithwallet`), and pass them to `generateblock` in the replacement
   blocks.
2. Log which txids were conflicted so tests can assert on them.

Effort: medium (raw-tx construction, only meaningful with spam enabled).

---

## 4. Dashboard / control panel

**What:** A small web UI (one container, compose profile `panel`, localhost-only) that
shows live chain state (height, block cadence, mempool depth/fees, current settings)
and lets the user change the tool settings — block cadence, miner weights, fee floor,
fill ratio, spam mode — and apply them with one click. Applying means rewriting the
values in `.env` and force-recreating only the affected service(s), i.e. automating
the manual flow documented in README "Retuning a live chain".

**Why it's a nice-to-have:** Retuning a live chain today means editing `.env` by hand
and knowing which compose service consumes which variable. That works, but a panel
makes the knobs discoverable, removes the docker knowledge requirement for teammates
using the simnet, and turns "try 3 different fee floors" from minutes of shell
round-trips into seconds. It also gives one place to watch the effect (mempool
histogram, block fullness) right next to the control that caused it.

**Implementation plan:**
1. Container with the project's `.env` bind-mounted and access to the Docker API
   (mounted `docker.sock` + docker CLI with the compose plugin) to run
   `docker compose up -d --force-recreate <service>`.
2. Backend (Rust axum to match the stack) that reads current values from `.env` plus
   defaults, validates edits, writes `.env`, and recreates only the services that
   consume the changed variables (the variable→service mapping is static, taken from
   docker-compose.yml).
3. Status pane fed by node1 RPC: height, last blocks with tx counts, mempool size and
   fee histogram, observed block interval.
4. Security: `docker.sock` is root-equivalent on the host, so bind the panel to
   localhost only and keep it out of the default profile.

Effort: medium (UI plus a thin compose/RPC glue layer; no changes to the existing
tools).

---

## 5. Chain snapshot/restore (UTXO set export/import)

**What:** Save the full state of a running chain — blocks, chainstate (the UTXO set) and
node wallets — into a portable archive, and restore it later into a fresh simnet:
`./scripts/snapshot.sh save <name>` / `./scripts/snapshot.sh restore <name>`. A restored
simnet boots already at the exported height and continues from there.

**Why it's a nice-to-have:** Every fresh `docker compose up` re-does the same bootstrap
work: mining 102 blocks for coinbase maturity, creating and funding the miner and
spammer wallets, building up a mempool. A snapshot does that work once; every later run
imports it and starts at block N with mature, spendable coins. And because the user's
keys live outside the simnet (node1 is wallet-disabled by design), the user's addresses
do not change between runs: coins received on the exported chain are still theirs after
a restore, so the user can fund their addresses once, snapshot, and rerun tests from
that state — "wait for bootstrap, then re-fund everything" becomes seconds. Snapshots
are also shareable: a bug report or a CI job can pin the exact chain state it needs.

**Implementation plan:**
1. Persist node datadirs on named volumes (mounted at `/home/bitcoin/.bitcoin` in each
   node container); today all chain state is ephemeral inside the containers, so this
   is the enabling change.
2. `snapshot.sh save <name>`: `docker compose stop` the nodes (clean shutdown flushes
   chainstate and wallets), run a scratch container that mounts the volumes and tars
   them to `snapshots/<name>.tar.gz` together with a small metadata file (image tag,
   height, relevant `.env` values), then restart.
3. `snapshot.sh restore <name>`: `docker compose down`, recreate the volumes, untar
   each datadir into its own node's volume, `docker compose up -d`. Nodes resume at the
   snapshot height already in consensus; electrs and the mempool stack keep no volumes
   and simply re-index from node1 on start.
4. Validate metadata on restore: warn when the bitcoind image or the node topology
   differs from the snapshot (datadir upgrades are one-way across major versions).

Why a datadir tar instead of Core's native `dumptxoutset`/`loadtxoutset`: assumeUTXO
only accepts snapshots whose base-block hash is hard-coded in the chain params, so an
arbitrary user chain on regtest is rejected — and it would not carry wallets, so the
miner and spammer funding would be lost. The datadir snapshot delivers the same outcome
(the UTXO set as of block N, maturity already done) plus the wallets, with no
consensus-level tricks.

Effort: small–medium (compose volume change plus a shell helper; no image or Rust
changes).

---

## Parked features

Designed but deliberately not built. Each entry records why it is parked and what would
revive it; the expensive design thinking is preserved in `parked/`.

### Fee-market simulation in the spammer — PARKED

**Status (2026-07-10): parked** — complexity/benefit says wait for a concrete
fee-estimation or fee-bumping test need. Full design (CPFP-safe per-branch fee ladder,
funding-pull deadlock fix) in [parked/fee-market-plan.md](parked/fee-market-plan.md),
which supersedes the implementation sketch that used to live here.

**What:** Make the spammer emit transactions with varied fee rates (sampled from a
configurable distribution, e.g. log-normal between `SPAM_FEE_MIN`/`SPAM_FEE_MAX` sat/vB)
and varied sizes/output counts, instead of identical 540-sat dust sends at fallback fee.

**Why it's a nice-to-have:** With uniform transactions, `estimatesmartfee`, mempool fee
histograms (visible in the mempool explorer) and any RBF/fee-bumping logic in the project
under test are meaningless, everything sits in one fee bucket. A spread of fee rates
creates real block-space competition: when spam volume exceeds block capacity, low-fee
transactions genuinely wait, which is exactly the mainnet behavior users want to
reproduce with the "tx per block" knob. Pairs well with the shipped Poisson block timing
(bursty blocks + fee spread = realistic mempool).

---

# Tech debt

- Build from sources instead of downloading binaries
