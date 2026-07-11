# Declarative scenarios

The `scenario` profile runs an ordered YAML history against an already running simchain
stack. It is intended for reproducible bug reports and downstream integration tests: a
successful run exits `0`; invalid input or the first failed step exits non-zero.

## Run a scenario

Start the ordinary stack so the mining controller can complete its fixed bootstrap,
then select a scenario for the one-shot orchestrator:

```bash
docker compose up -d
SCENARIO_FILE=scenarios/reorg-during-sync.yml \
  docker compose --profile scenario run --rm --build btc-simnet-scenario
```

The engine waits for node1 RPC and height 204 before executing step 1. Pre-bootstrap
history changes are deliberately unsupported because bootstrap funding stages are keyed
to fixed heights. Restore a post-bootstrap snapshot first when a test needs an exact
starting state.

The profile mounts the repository at `/workspace` and the host Docker socket. The latter
is root-equivalent host access, which is why `scenario` is opt-in and not included in
`all-tools`.

## Schema

Every file has exactly version 1 and an ordered `steps` list. Unknown fields and step
types are rejected.

```yaml
version: 1
steps:
  - type: wait_height
    height: 260

  - type: sleep
    secs: 5

  - type: pause_mining

  - type: mine
    node: btc-simnet-node2
    blocks: 3

  - type: spam_burst
    node: btc-simnet-node2
    txs: 100
    outputs_per_tx: 25

  - type: reorg
    depth: 2
    empty: false

  - type: partition
    node: btc-simnet-node3
    main_blocks: 3
    isolated_blocks: 4

  - type: resume_mining
```

Validation rules:

- `wait_height.height` is at least 204.
- `sleep.secs`, `mine.blocks`, `reorg.depth`, `spam_burst.txs`, and both partition
  block counts are positive.
- Miner nodes are `btc-simnet-node2` or `btc-simnet-node3`.
- `spam_burst.outputs_per_tx` is non-negative. Zero selects sequential
  `sendtoaddress`; a positive value selects `sendmany` with that many 546-sat outputs.
- Partition branch lengths differ so the winner is deterministic.
- `reorg.empty` defaults to `false` when omitted; all other fields are required.

## Step behavior

`wait_height` polls node1 every 500 ms. `sleep` is a wall-clock delay.
`pause_mining` stops `btc-simnet-mining-controller`; `resume_mining` recreates/starts it
and waits until Docker reports it running. `mine` gets a fresh address from the selected
miner wallet and calls `generatetoaddress` directly.

`reorg` delegates to `scripts/simulate-reorg.sh`, and `partition` delegates to
`scripts/partition.sh run`; those operator interfaces remain the single authority for
their behavior. `spam_burst` uses the selected wallet directly and stops at its first
rejected transaction. The equivalent manual helper is:

```bash
./scripts/spam-burst.sh btc-simnet-node2 --txs 100 --outputs-per-tx 25
```

Each step logs its index, type, parameters, and elapsed time. A successful run logs the
final node1 height and best-block hash.

## Failure and cleanup

Execution stops at the first failure. If the engine paused mining earlier in the file,
it best-effort restarts the controller before exiting. If a partition command fails, it
best-effort invokes `partition.sh heal` for the target first. Cleanup errors are logged
separately and do not replace the original step error. A failed explicit `resume_mining`
is not retried by cleanup, so its real failure is not hidden behind an identical retry.

The same 30-minute default timeout applies to RPC readiness, bootstrap/height waits, and
controller restart. Override it with `SCENARIO_TIMEOUT_SECS`.

## Result artifacts and CI

Set `SCENARIO_RESULT_FILE` to write a JSON summary. A path under `/workspace` persists
in the host checkout:

```bash
SCENARIO_FILE=scenarios/reorg-during-sync.yml \
SCENARIO_RESULT_FILE=/workspace/results/reorg.json \
  docker compose --profile scenario run --rm btc-simnet-scenario
```

The artifact contains success/failure, executed and total step counts, elapsed
milliseconds, final height/hash when reachable, and the first error. `compose run`
propagates the one-shot container's exit code, making the command a CI job boundary.

## Shipped examples

- `scenarios/pause-then-burst.yml` pauses background mining, creates a batch-wallet
  mempool burst, then resumes mining.
- `scenarios/reorg-during-sync.yml` creates a two-block reorganization and allows a
  brief observation window.
- `scenarios/partition-node3.yml` isolates node3, mines unequal branches, heals, and
  waits for convergence through the shared partition helper.
