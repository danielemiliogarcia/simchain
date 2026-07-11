# Chain snapshot/restore: design and implementation record

## Status: IMPLEMENTED (2026-07-10, verification plan §8 executed in full)

Implements the former nice-to-have **"Chain snapshot/restore (UTXO set
export/import)"** (removed from [NICE-TO-HAVE.md](NICE-TO-HAVE.md) on ship, per repo
convention). The user-facing rationale, and the argument for a datadir snapshot over
Core's native `dumptxoutset`/`loadtxoutset`, are preserved in the
[appendix](#appendix-rationale-from-the-original-nice-to-have-entry). The sections
below are the design as built: exact changes, file by file, plus the verification
plan that was run.

## 1. Goal and non-goals

**Goal:** two commands.

```bash
./scripts/snapshot.sh save mysnap      # running chain -> snapshots/mysnap.tar.gz
./scripts/snapshot.sh restore mysnap   # fresh simnet resuming at the saved height
```

A restored simnet boots with the bootstrap already done (chain past height 204, mature
miner wallets, funded user address, persisted mempool) and simply continues: the
mining controller resumes block production on top, the spammer resumes spamming, and
coins the user received on the saved chain are still spendable with their external
keys.

**Non-goals:**

- No cross-version migration. A snapshot is restored onto the same `BTC_IMAGE` it was
  taken from (bitcoind datadir upgrades are one-way across major versions). The script
  warns on mismatch; making it work is not attempted.
- No live/hot snapshot. The stack is stopped for the seconds the tar takes; leveldb
  (chainstate, indexes) is not safely copyable under a running bitcoind.
- No snapshotting of the optional tools' state. electrs keeps its DB in container-local
  `/tmp/electrs-db` and re-indexes from node1 on start; the mempool explorer's mariadb
  is likewise ephemeral and rebuilds. Explorer statistics history is lost on restore —
  accepted.

## 2. Current state (what the plan builds on)

- **No volumes anywhere.** `docker-compose.yml` declares zero volumes; all three node
  datadirs live inside the containers, so today `docker compose down` destroys the
  chain. Persisting the datadirs is the enabling change.
- **Datadir path is `/home/bitcoin/.bitcoin` in both images** — the official
  `bitcoin/bitcoin:31.1` and the locally built one (`docker/bitcoin-node.Dockerfile`
  sets `ENV BITCOIN_DATA=/home/bitcoin/.bitcoin`). The local image's entrypoint
  already `chown -R`s the datadir on every start, so restored file ownership
  self-heals.
- **Bootstrap is height-driven and resumable.** `crates/mining-controller/src/bootstrap.rs`
  ends at `BOOTSTRAP_END = 204` and skips every stage whose target height is already
  reached: on a restored chain it logs "Chain already bootstrapped" and goes straight
  to steady-state mining. `setup_wallet` (`crates/mining-controller/src/wallet.rs`)
  loads an existing wallet instead of creating one. **No Rust changes are needed.**
- **All persistent state lives in the node datadirs**: blocks, chainstate, `-txindex`
  data, the `node2`/`node3` wallets (used by both the controller and the spammer), and
  `mempool.dat` (written on clean shutdown, loaded on start). The controller and
  spammer containers are stateless by design (their compose comments say so).
- **Wallet names and the user address come from `.env`**
  (`NODE2_WALLET_NAME`/`NODE3_WALLET_NAME`, `USER_ADDRESS`): a snapshot is only
  meaningful under the same values, which is what the metadata check (§5) enforces.

## 3. Change 1 — persist node datadirs on named volumes

`docker-compose.yml`: add one volume per node, explicitly named (the project already
pins container and network names, and explicit names free the snapshot script from
depending on the compose project name):

```yaml
# in each node service
    volumes:
      - node1-data:/home/bitcoin/.bitcoin   # node2-data / node3-data respectively

# top level
volumes:
  node1-data:
    name: btc-simnet-node1-data
  node2-data:
    name: btc-simnet-node2-data
  node3-data:
    name: btc-simnet-node3-data
```

**This is a behavior change independent of snapshots** and must be documented (§7):
after `docker compose down && docker compose up` the chain now *persists* (bitcoind
reloads the datadir, the controller skips the bootstrap). A fresh chain now requires
`docker compose down -v`. This is a feature — it is exactly what makes restore
possible — but the README quick-start must say it. For the old "disposable chain"
workflow, `scripts/fresh-chain.sh` wraps `down -v` + `up -d` in one command; compose
itself has no flag for this (`up --renew-anon-volumes` only touches *anonymous*
volumes, and anonymous volumes would leak on every cycle and hide the datadirs from
the snapshot script).

Only the three node services get volumes. Controller, spammer and reorg stay
stateless; electrs and the mempool stack stay ephemeral on purpose (§1 non-goals).

## 4. Change 2 — `scripts/snapshot.sh`

New script, same conventions as `scripts/simulate-reorg.sh` (resolve `REPO_ROOT`, run
`docker compose -f "$REPO_ROOT/docker-compose.yml" --project-directory "$REPO_ROOT"`).
Reads `.env` for RPC credentials with the same defaults as the compose file
(`BTC_RPC_USER:-foo`, `BTC_RPC_PASS:-rpcpassword`).

```
Usage:
  snapshot.sh save <name>              stop stack, tar volumes, restart stack
  snapshot.sh restore <name> [--force] wipe volumes, untar, start stack
  snapshot.sh list                     show snapshots with height/date/image
```

`<name>` must match `[A-Za-z0-9._-]+`. Output is two files:
`snapshots/<name>.tar.gz` (the three datadirs under top-level `node1/`, `node2/`,
`node3/`) and `snapshots/<name>.json` (metadata, §5). Add `snapshots/` to
`.gitignore`.

### save

1. Refuse to overwrite an existing snapshot name.
2. While the stack is still up, collect metadata: height and best block hash via
   `docker exec btc-simnet-node1 bitcoin-cli -regtest -rpcuser=... getblockcount`
   / `getbestblockhash`, plus the resolved `BTC_IMAGE`, wallet names and
   `USER_ADDRESS` (defaults mirrored from the compose file). If the stack is not
   running, abort with a clear message — snapshotting a stopped stack is possible but
   the height/hash sanity data would be missing; keep v1 simple.
3. Record the currently *running* services (`docker compose ps --services --status
   running`): the resume step must `start` exactly these — starting the whole profile
   trips over containers that were never created (e.g. the mempool stack when only
   the basic profile is up; found in testing).
4. `docker compose stop` — the whole stack, not just the nodes: bitcoind gets SIGTERM
   and flushes chainstate, wallets and `mempool.dat`; stopping the controller/spammer
   with the nodes avoids their `restart: on-failure` crash-looping against dead RPC.
5. Tar the volumes from a scratch container (no dependencies beyond docker):

   ```bash
   docker run --rm \
     -v btc-simnet-node1-data:/snap/node1:ro \
     -v btc-simnet-node2-data:/snap/node2:ro \
     -v btc-simnet-node3-data:/snap/node3:ro \
     -v "$REPO_ROOT/snapshots:/out" \
     alpine tar czf "/out/<name>.tar.gz" --numeric-owner -C /snap node1 node2 node3
   ```

6. Write `snapshots/<name>.json`, then `docker compose start <running services>` to
   resume exactly what was running before.

### restore

1. Require both snapshot files; read the metadata.
2. Validate against the current environment (§5). On mismatch print what differs and
   abort; `--force` proceeds anyway.
3. `docker compose down --remove-orphans` (volumes survive `down` without `-v`), then
   `docker volume rm` the three volumes (ignore not-found).
4. `docker compose create`: compose recreates the volumes (with its own labels, so no
   "created outside of compose" warnings later) and the containers, but starts
   nothing — the datadirs are still empty at this point.
5. Untar from a scratch container (same mounts as save, `rw`, `tar xzf ... -C /snap
   --numeric-owner`).
6. `docker compose up -d <saved services>`: the metadata records which services were
   running at save time, and naming them explicitly makes compose activate their
   profiles automatically — the snapshot's shape (tools included) comes back with no
   flags. Extra args the user passed after the name override the recorded shape and
   are inserted as compose *global* flags (before the subcommand, also on the
   `create` above), so `snapshot.sh restore mysnap --profile all-tools` works;
   snapshots without the field fall back to the default services.
7. Post-check: wait for node1 healthy, then assert `getblockcount >=` the metadata
   height (the controller may already be mining on top — growth is success, shrinkage
   is a failed restore) and that `getblockhash <meta-height>` equals the metadata best
   hash. Print the resumed height.

### list

For each `snapshots/*.json`: name, creation date, height, image. Plain columns.

## 5. Metadata and restore validation

`snapshots/<name>.json`:

```json
{
  "name": "mysnap",
  "created": "2026-07-10T15:04:05-03:00",
  "height": 412,
  "best_block_hash": "3ba3...",
  "btc_image": "bitcoin/bitcoin:31.1",
  "node2_wallet": "node2",
  "node3_wallet": "node3",
  "user_address": "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr",
  "services": "btc-simnet-mining-controller btc-simnet-node1 ... mempool-web electrs"
}
```

`services` is the running-services list captured at save time (§4 save step 3),
reused by restore to bring the stack back in the same shape.

Checks on restore, in decreasing severity:

- **`btc_image` differs** → abort (datadir format may not round-trip; upgrades are
  one-way). `--force` overrides for e.g. a rebuilt local image with the same bitcoind
  version.
- **Wallet names differ** → abort. The controller/spammer would create *new* empty
  wallets and mine/spam from zero balance next to the funded ones.
- **`user_address` differs** → loud warning (not abort): the chain's user funding
  (bootstrap blocks 3–4 and anything sent since) pays the *old* address. This is the
  core use case — the whole point of restoring is that the user's address set did not
  change — so the script must make an accidental mismatch impossible to miss.

## 6. What needs no changes (verify, don't build)

- **Mining controller**: `bootstrap.rs:53` skips the funding sequence when
  `height >= BOOTSTRAP_END`; `wallet.rs` loads existing wallets. Resumes rotation
  mining directly.
- **Spammer**: stateless between cycles; funds itself from the restored miner wallets
  and rebuilds its fan-out from on-chain/mempool state exactly as it does after a
  container restart today.
- **Reorg simulator**: one-shot, reads live chain state; indifferent to how the chain
  got there.
- **electrs / mempool explorer**: no volumes; re-index from node1 on start. First
  start after a restore takes a few seconds of `--jsonrpc-import` catch-up.
- **mempool.dat**: restored transactions re-enter the mempool on start. Note:
  bitcoind expires mempool txs older than 336 hours; a very old snapshot restores
  with an empty mempool — harmless, the spammer refills it.

## 7. Documentation updates (done in the same change)

- **README**: "How to run" tear-down notes now cover `down` (chain persists) vs
  `down -v` (fresh chain); new "Chain snapshots" section; troubleshooting reset
  recipe updated; this document linked from the Documents list.
- **docs/RUNBOOK.md**: snapshots recipe added.
- **docs/SNAPSHOTS.md**: cookbook with concrete calls for the common situations.
- **docs/NICE-TO-HAVE.md**: feature #5 removed on ship (repo convention: shipped
  items are removed, not marked done); its rationale preserved in the appendix here.
- **`.gitignore`**: `snapshots/` added.

## 8. Verification plan (manual, in order — executed 2026-07-10, all green)

1. Fresh start with the volume change, no snapshot involved: `docker compose up`,
   bootstrap completes to 204+, spam flows. Then `docker compose down && up`: chain
   resumes (no re-bootstrap, controller logs "Chain already bootstrapped"), height
   continues. `down -v && up`: fresh chain, full bootstrap runs. This validates §3
   alone.
2. `save`: on a running chain past bootstrap, send coins to a user-controlled address,
   note height H and the user UTXOs (`scantxoutset`). Run
   `./scripts/snapshot.sh save t1` — stack stops, tar + json appear, stack resumes and
   keeps mining.
3. `restore` onto a wiped world: `docker compose down -v`, then
   `./scripts/snapshot.sh restore t1`. Assert: node1 healthy; height ≥ H and
   `getblockhash H` matches metadata; `scantxoutset` still shows the user UTXOs;
   controller mines on top within one block interval; spammer resumes (mempool
   refills); no wallet-creation errors in controller/spammer logs.
4. Tools after restore: `restore t1 --profile mempool` — electrs re-indexes, explorer
   shows the restored chain.
5. Guard rails: `restore` with a changed `NODE2_WALLET_NAME` in `.env` aborts;
   `--force` proceeds; changed `USER_ADDRESS` prints the loud warning; `save` refuses
   a duplicate name; `restore` of a missing name fails cleanly.
6. Reorg on a restored chain: `./scripts/simulate-reorg.sh 3` behaves normally.

## 9. Risks and edge cases

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

## 10. Effort and change list

Small–medium. No Rust changes, no image changes.

| File | Change |
| --- | --- |
| `docker-compose.yml` | 3 volume mounts + top-level `volumes:` block + `stop_grace_period` on nodes |
| `scripts/snapshot.sh` | new (~150 lines of bash: arg parsing, metadata, tar in/out, checks) |
| `scripts/fresh-chain.sh` | new: one-command `down -v` + `up -d` for the old disposable-chain workflow |
| `.gitignore` | `snapshots/` |
| `README.md` | persistence note + Snapshots section |
| `docs/RUNBOOK.md` | save/restore recipe |
| `docs/NICE-TO-HAVE.md` | remove feature #5 on ship |

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
