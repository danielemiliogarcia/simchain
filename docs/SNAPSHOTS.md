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
