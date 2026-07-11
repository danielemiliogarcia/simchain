# Code review: `chain-snapshots` (d3915b2 "feat: snapshots")

Implements `docs/snapshot-restore-plan.md`. Reviewed against that plan and the
compose/scripts conventions on master.

**Verdict: solid, ship-worthy.** The design decisions the plan promised are all
there and correctly executed: named volumes with explicit names, clean-stop
before tar, height/hash metadata captured pre-stop but validated via
`getblockhash <saved-height>` (so blocks mined between the RPC call and the
stop don't break the check), service-shape recording so restore brings back the
exact profile set with no flags. No blocking bugs found. Findings below are
robustness/polish items.

## Findings

### 1. MEDIUM — restore: failed `docker volume rm` is silently swallowed

`scripts/snapshot.sh` (`cmd_restore`):

```bash
docker volume rm "$v" >/dev/null 2>&1 || true
```

The `|| true` is meant for "no such volume", but it also swallows
"volume is in use" (a container outside the compose project, e.g. a helper the
user launched, still holding it). The script then unarchives *into the old
volume*: tar overwrites files present in the archive but leaves newer/extra
files behind (later `blk*.dat`, wallet dirs), producing a datadir that is
neither the snapshot nor the old chain. The post-check (hash at saved height)
catches a *different* chain but not a contaminated continuation of the same
one. Fix: capture stderr and only ignore not-found, e.g.

```bash
out=$(docker volume rm "$v" 2>&1) || { echo "$out" | grep -qi 'no such volume' || die "cannot remove $v: $out"; }
```

### 2. LOW — save: a failure mid-save leaves the stack stopped

Between `compose stop` and `compose start $running_services` there is no error
trap. If the `docker run … tar czf` step fails (disk full, image pull failure),
`set -e` exits the script and the whole simnet stays down with no message about
how to resume. A `trap 'compose start $running_services' ERR`-style guard (or
at least a die message saying "stack is stopped, run docker compose start")
would make the failure mode self-explanatory.

### 3. LOW — snapshot files end up owned by root

The tar runs in an alpine container as root with `$SNAP_DIR` bind-mounted, so
`snapshots/<name>.tar.gz` is root-owned on the host: the user can `list` and
`restore` but cannot delete or move snapshots without sudo. The container must
be root to *read* the datadirs, but a final `chown $(id -u):$(id -g)
/out/<name>.tar.gz` inside the same `docker run` (or a second trivial one)
fixes the output file.

### 4. LOW — `.env` parsing does not strip quotes

`env_get` returns the raw value after `=`. docker compose strips surrounding
quotes in `.env` (`BTC_IMAGE="bitcoin/bitcoin:31.1"` resolves unquoted), but
the script would carry the quotes into `bcli` credentials and into the
metadata, causing wrong-credential failures on save or a false
`BTC_IMAGE differs` abort on restore. Users probably don't quote these today,
but the divergence from compose semantics is the kind of thing that costs an
hour when someone does. One `sed -e 's/^"\(.*\)"$/\1/'` in `env_get` closes it.

### 5. NOTE — `meta_get` is a hand-rolled JSON parser

The `sed` expression works for the exact one-key-per-line format `cmd_save`
writes, including unquoted numbers and the space-separated `services` string —
verified against the emitted format. It will break the moment anyone
pretty-prints or hand-edits the metadata. Acceptable for a self-generated
file (and it keeps the zero-dependency promise); just don't edit those files.

### 6. NOTE — commit hygiene: four unrelated plan documents bundled in

The commit adds `dashboard-control-panel-plan.md`,
`declarative-scenario-engine-plan.md`, `network-partition-latency-plan.md`,
and `reorg-double-spend-plan.md` (~2,300 lines) alongside the snapshots
feature. They're the basis of the sibling branches, so landing them here makes
the stack build, but "feat: snapshots" is not where a reader will look for
them. Worth a separate `docs:` commit next time.

## Plan conformance

Checked section by section against `docs/snapshot-restore-plan.md`:

- **§3 volumes** — exact match: three named volumes, explicit
  `btc-simnet-*-data` names, mounts only on the node services,
  `stop_grace_period: 60s` on all three nodes (plan §9). Controller, spammer,
  reorg, electrs and the mempool stack stay stateless as specified.
- **§4 save/restore/list** — all steps present, including the details that are
  easy to drop: refuse duplicate names, abort when node1 isn't running,
  running-services capture before `stop`, `create` before untar to avoid the
  "created outside of compose" label warnings, `--numeric-owner` both ways,
  user compose flags overriding the recorded service shape.
- **§5 guard rails** — image and wallet-name mismatches abort (with `--force`
  override), `USER_ADDRESS` mismatch warns loudly without aborting. Matches
  the severity ordering in the plan.
- **§7 docs** — README, RUNBOOK, new SNAPSHOTS.md cookbook, NICE-TO-HAVE item
  removed (not marked done), `.gitignore` entry. All present.
- **`fresh-chain.sh`** — matches the plan's rationale (compose has no native
  "renew named volumes" flag); tears down with the widest profile so tool
  containers can't survive as orphans.

Deviation worth noting: none found. The script grew to ~277 lines vs the
plan's ~150 estimate, entirely in error handling and comments — fine.

## Good stuff (kept short)

- The save-time race (blocks mined between `getblockcount` and `compose stop`)
  is designed away by validating `getblockhash <saved-height>` and allowing
  `height >= saved` on restore — this is the correct invariant, not a lazy one.
- Recording running services and passing them to `up -d` so compose activates
  profiles implicitly is a neat trick that removes a whole class of
  "restored but half the stack is missing" bugs.
- The plan's appendix argument for datadir-tar over `dumptxoutset` is
  technically accurate (assumeUTXO hash pinning in chain params, provisional
  chainstate, UTXO-only payload) and the implementation is consistent with it.