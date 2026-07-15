# Declarative scenarios

Scenarios are validated and executed as durable jobs by the Simchain control plane. The
client that submits a file may exit or disconnect without cancelling execution. At most
one mutation job runs at a time, and each job records ordered progress events, owned
leases, checkpoints, its result, and cleanup outcome.

## Run a scenario

Start the simnet and control plane, then submit a version-1 YAML file with the first-party
client:

```bash
docker compose --profile control-plane up -d --build
cargo run -p simchainctl -- scenario run scenarios/reorg-during-sync.yml \
  --result results/reorg.json
```

`scenario run` uploads the file, streams new events, waits for the terminal state, and
uses stable automation exit codes. The optional result artifact contains the complete
terminal job, checkpoint summaries, and persisted event history.

The historical one-shot Compose command remains a thin compatibility client. Its image
has no Docker CLI or Docker socket; it submits the same server-side job:

```bash
SCENARIO_FILE=scenarios/reorg-during-sync.yml \
  docker compose --profile scenario run --rm --build btc-simnet-scenario
```

Every scenario waits for node1 to reach bootstrap height 204 before step 1. Pre-bootstrap
history mutation remains unsupported because bootstrap funding stages use fixed heights.

## Schema

Every file has exactly `version: 1` and an ordered `steps` list. Unknown fields and step
types are rejected before the mutation coordinator is reserved. Existing version-1 files
remain valid.

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

  - type: checkpoint
    name: mempool_loaded
    timeout_secs: 600

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
- `spam_burst.outputs_per_tx` may be zero. Zero uses sequential `sendtoaddress`; a
  positive value uses `sendmany` with that many 546-sat outputs.
- Partition branch lengths differ so the winner is deterministic.
- Checkpoint names are non-empty, URL-safe, at most 100 bytes, and unique in one file.
- Checkpoints pause by default. A pausing checkpoint requires a positive `timeout_secs`;
  `pause: false` records a durable milestone and continues immediately.

## Checkpoints and CI

On checkpoint arrival, the server durably records a unique generation and a full live
chain/mining/spam summary before exposing the reached state. A pausing checkpoint then
waits for a matching release, cooperative abort, or its declared timeout. Release is
idempotent for the same generation; stale generations return a conflict.

The shipped [ci-checkpoint.yml](../scenarios/ci-checkpoint.yml) supports the intended CI
barrier flow:

```bash
job="$(cargo run --quiet -p simchainctl -- \
  scenario start scenarios/ci-checkpoint.yml --id-only)"
trap 'cargo run --quiet -p simchainctl -- jobs abort "$job" >/dev/null 2>&1 || true' EXIT

cargo run --quiet -p simchainctl -- \
  scenario wait "$job" --checkpoint mempool_loaded --timeout 600

# Assert the downstream system while mining remains held at this exact state.
cargo test -p downstream-integration

cargo run --quiet -p simchainctl -- scenario release "$job" mempool_loaded
cargo run --quiet -p simchainctl -- jobs watch "$job" --timeout 900
trap - EXIT
```

Killing the waiting client does not affect the server job. Another client can inspect or
release the checkpoint, or the checkpoint timeout will fail the job and trigger cleanup.

## Action and cleanup behavior

Height waits, manual mining, and wallet bursts use Bitcoin RPC directly. Mining pause and
resume use an expiring job-owned worker lease. Reorg steps use both mining and spam leases,
the reusable reorg executor, and strict node1 witness convergence. Partition steps lease
the namespace-local target network agent, block P2P ingress and egress, mine both branches,
heal, and witness the deterministic winner before worker leases can resume. There is only
one public backend: the control plane.

Execution stops at the first failed step. Cleanup releases only resources the scenario
acquired, reports cleanup errors separately from the primary failure, and retains the
mutation lock if safe recovery is still pending. Cleanup heals network impairment and
witnesses convergence before releasing spam and mining. A control-plane restart marks an
active scenario interrupted and clears or safely recovers its owned network/worker leases
before another mutation may begin.

## Shipped examples

- `pause-then-burst.yml` pauses background mining, creates a wallet burst, then resumes.
- `reorg-during-sync.yml` creates a two-block reorganization and observation delay.
- `partition-node3.yml` builds unequal branches across a temporary partition.
- `ci-checkpoint.yml` holds a deterministic mempool state for external assertions.
