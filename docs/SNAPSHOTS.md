# Snapshot cookbook

Concrete `./scripts/snapshot.sh` calls for the common situations. What a snapshot is
and how it works under the hood: [snapshot-restore-plan.md](snapshot-restore-plan.md);
the short version lives in the README "Chain snapshots" section.

```bash
# --- Basic cycle ---------------------------------------------------------

# Archive the running chain (stack must be up). Stops everything cleanly,
# tars the 3 node datadirs, writes metadata, resumes the stack.
./scripts/snapshot.sh save baseline

# See what you have saved: name, height, date, bitcoind image.
./scripts/snapshot.sh list

# Bring the chain back exactly as saved (wipes current chain volumes first).
# Also restarts the same services that were running at save time -- tool
# profiles included, no flags needed.
./scripts/snapshot.sh restore baseline


# --- Typical workflow: reusable funded test state ------------------------

# 1. Fresh chain, wait for bootstrap (height 204), send coins to your
#    addresses, let some blocks confirm...
# 2. Freeze that state:
./scripts/snapshot.sh save funded-wallets

# 3. Run destructive tests against the chain (reorgs, double-spends, ...).
./scripts/simulate-reorg.sh 5

# 4. Chain trashed? Back to the funded state in ~30 s, no re-mining,
#    no re-funding -- your keys still spend the same UTXOs:
./scripts/snapshot.sh restore funded-wallets


# --- Controlling which services come up ------------------------------------

# The snapshot remembers its shape: saving while the explorer stack runs
# means restore brings the explorer stack back automatically. To override
# the recorded shape, append docker compose flags (they win over the
# metadata); this restores a basic-profile snapshot WITH the explorer:
./scripts/snapshot.sh restore baseline --profile mempool

# Everything, regardless of what was running at save time:
./scripts/snapshot.sh restore baseline --profile all-tools


# --- Guard rails ----------------------------------------------------------

# Restore refuses if .env changed since the save (BTC_IMAGE or wallet
# names differ). Override only when you know the datadir is compatible,
# e.g. same bitcoind version rebuilt under another tag:
./scripts/snapshot.sh restore baseline --force

# Changed USER_ADDRESS in .env? Restore proceeds but warns loudly:
# the chain's user funds still belong to the address saved in the snapshot.


# --- Fresh chain ----------------------------------------------------------

# The chain persists across down/up now (that is what makes snapshots work),
# so "just up for a fresh chain" is gone. One-command replacement -- wipes
# the volumes and starts over (flags go to docker compose):
./scripts/fresh-chain.sh
./scripts/fresh-chain.sh --profile mempool

# About to wipe but might want this chain back later? Snapshot it first:
./scripts/snapshot.sh save keep-me && ./scripts/fresh-chain.sh


# --- Housekeeping ---------------------------------------------------------

# Snapshots live in ./snapshots/ (gitignored). Alternate location:
SNAPSHOT_DIR=/mnt/big-disk/simchain-snaps ./scripts/snapshot.sh save baseline

# Delete a snapshot: just remove its two files.
rm snapshots/baseline.tar.gz snapshots/baseline.json
```

One rule to remember: `save` needs the stack running; `restore` doesn't care — it
rebuilds the stack from scratch.

## What survives a snapshot, and what doesn't

| State | Survives? | How |
| --- | --- | --- |
| Blocks, UTXO set, txindex | yes | node datadirs in the archive |
| Miner wallets (node2/node3) | yes | live in the node datadirs |
| Unconfirmed (in-flight) txs | yes | bitcoind writes `mempool.dat` on the clean stop; reloaded on start |
| User coins | yes | on-chain; user keys are external, so the same keys keep spending them |
| Spammer/controller memory | no, by design | both are stateless: the controller re-reads the height, the spammer resyncs its branch UTXO set from the chain (`scantxoutset`) and the reloaded mempool |
| electrs / explorer DB | no, by design | ephemeral; they re-index from node1 in seconds on start |


## Risks and edge cases

- **In-flight writes at stop time**: `docker compose stop` default grace is 10 s;
  bitcoind normally flushes well within it, but set `stop_grace_period: 60s` on the
  node services in the same PR so a slow flush (large mempool) is never killed
  mid-write.
- **Ownership across images**: official image and local image may use different UIDs
  for the `bitcoin` user. `--numeric-owner` preserves whatever the snapshot had, and
  the local entrypoint re-chowns on start; the official image handles its own
  permissions. If a restore onto the *other* image is ever forced, ownership is the
  first thing to check.
- **Disk growth**: with volumes, chain data now accumulates on the host across
  restarts (DATA-mode spam writes ~full blocks). `down -v` is the reset;
  `snapshot.sh list` plus `du -sh snapshots/` keep it visible. Not mitigated further
  in v1.
- **Compose `start` vs profiles**: after `save`, `docker compose start` only restarts
  containers that already existed, so whatever profile set was running resumes
  unchanged — no profile bookkeeping needed in the script.


## Appendix: rationale (from the original nice-to-have entry)

**What:** Save the full state of a running chain — blocks, chainstate (the UTXO set)
and node wallets — into a portable archive, and restore it later into a fresh simnet.
A restored simnet boots already at the exported height and continues from there.

**Why:** Every fresh `docker compose up` re-does the same bootstrap work: mining to
height 204 for funding plus coinbase maturity, creating and funding the miner wallets,
building up a mempool. A snapshot does that work once; every later run imports it and
starts at block N with mature, spendable coins. And because the user's keys live
outside the simnet (node1 is wallet-disabled by design), the user's addresses do not
change between runs: coins received on the exported chain are still theirs after a
restore, so the user can fund their addresses once, snapshot, and rerun tests from
that state — "wait for bootstrap, then re-fund everything" becomes seconds. Snapshots
are also shareable: a bug report or a CI job can pin the exact chain state it needs.

**Why a datadir tar instead of Core's native `dumptxoutset`/`loadtxoutset`:** those
RPCs are the assumeUTXO feature, built to speed up initial block download on mainnet,
and its design fits that goal, not this one:

- **Arbitrary snapshots are rejected by design.** So that users never have to trust a
  downloaded UTXO set, `loadtxoutset` only accepts a snapshot whose base-block hash is
  hard-coded in Core's chain params (`m_assumeutxo_data`), where each entry also pins
  the expected hash of the serialized UTXO set. Regtest ships only a couple of fixed
  test vectors used by Core's own functional tests, and there is no runtime option to
  add entries. A chain simchain mined has different block hashes by construction, so
  its snapshot can never match: `dumptxoutset` happily exports it, but no stock
  bitcoind will import it — that would take patching the chain params and rebuilding
  Core.
- **The loaded snapshot is provisional, not final.** Even on a chain with a matching
  entry, assumeUTXO treats the imported chainstate as unvalidated: the node keeps
  downloading and re-verifying every block from genesis in the background and only
  then promotes the snapshot chainstate. In a restored simnet no peer has those
  historical blocks anymore, so background validation could never complete.
- **It carries the UTXO set and nothing else.** No wallets, so the miner and spammer
  funding is gone and the spam pipeline is dead on arrival; no raw blocks, so
  `getblock` fails and electrs / the mempool explorer cannot index a chain whose
  blocks do not exist; no `-txindex` data; no mempool contents.

The datadir tar sidesteps all three problems: it is the node's own state
byte-for-byte — blocks, chainstate, wallets, txindex, even `mempool.dat` — so there is
no trust question to answer, no background validation to wait for, and every RPC and
downstream indexer works immediately after restore. It delivers exactly what the UTXO
export was after (the UTXO set as of block N, coin maturity already done) plus
everything around it, with no consensus-level tricks.


## Q&A

**Is the mempool saved?** Yes. `save` stops bitcoind with SIGTERM; on a clean
shutdown bitcoind writes the whole mempool to `mempool.dat` in the datadir, which is
inside the archive. On restore it reloads it. Observable after a restore: the first
mined blocks are full of the reloaded transactions.

**What happens to in-flight (unconfirmed) transactions?** No loss window exists.
Compose stops services in reverse dependency order — the spammer and controller die
*before* the nodes — so nothing can broadcast while the nodes shut down. Every
unconfirmed tx at save time is in some node's mempool, lands in that node's
`mempool.dat`, and comes back unconfirmed after restore; the controller then mines it
normally. Each node saves its *own* mempool view; tiny divergences re-sync over P2P
within seconds. One caveat: bitcoind expires mempool transactions older than 336
hours, so restoring a very old snapshot can come back with an empty mempool —
harmless, the spammer refills it.

**How does the spammer recover if it loses its in-memory maps?** Non-issue by
design: a restore is indistinguishable from a container crash-restart, which the
spammer already handles. The raw engine has an explicit recovery path
(`resync()` → `scan_address_utxos()` in
[crates/spammer/src/raw_transaction_spammer.rs](../crates/spammer/src/raw_transaction_spammer.rs)):
it rebuilds its branch UTXO set from the chain via `scantxoutset`, filtering outputs
already spent by its own still-in-mempool transactions with
`gettxout(include_mempool)`. What it loses is only ephemeral bookkeeping (branch
cursors, RBF shape cache); the funds are in the miner wallets and on-chain. After a
restore, spam resumes and the mempool refills without intervention.

