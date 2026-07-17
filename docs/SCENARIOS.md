# Declarative scenarios

Scenarios are validated and executed as durable jobs by the Simchain control plane. The
client that submits a file may exit or disconnect without cancelling execution. At most
one mutation job runs at a time, and each job records ordered progress events, owned
leases, checkpoints, its result, and cleanup outcome.

## Run a scenario

Start the simnet and control plane, then submit a version-1 YAML file with the first-party
client:

```bash
docker compose up -d --build
cargo run -p simchainctl -- scenario validate scenarios/reorg-during-sync.yml
cargo run -p simchainctl -- scenario explain scenarios/reorg-during-sync.yml
cargo run -p simchainctl -- scenario run scenarios/reorg-during-sync.yml \
  --result results/reorg.json
```

`scenario validate` and `scenario explain` parse the file locally without uploading it.
`scenario run` uploads the file, streams new events, waits for the terminal state, and
uses stable automation exit codes. The optional result artifact contains the complete
terminal job, checkpoint summaries, and persisted event history.

Every scenario waits for node1 to reach bootstrap height 204 before step 1. Pre-bootstrap
history mutation remains unsupported because bootstrap funding stages use fixed heights.

Scenarios cover hot control-plane actions: runtime retuning, config assertions, faucet
funding, manual mining, spam bursts, reorgs, partitions, timed degradation, and
checkpoints. They still do not own Docker lifecycle or chain-volume deletion. Start from
a fresh chain outside the control plane when the test requires one, then run the scenario.

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

  - type: set_config
    settings:
      BLOCK_INTERVAL_MODE: fixed
      BLOCK_INTERVAL_MEAN_SECS: 10
      SPAM_FILL_BLOCK_RATIO: 4
      SPAM_FEE: 0.002

  - type: assert_config
    effective: true
    settings:
      BLOCK_INTERVAL_MODE: fixed
      BLOCK_INTERVAL_MEAN_SECS: 10
      SPAM_FILL_BLOCK_RATIO: 4
      SPAM_FEE: 0.002

  - type: wait_until
    timeout_secs: 120
    condition:
      kind: component
      component: spam
      status: active

  - type: wait_tx
    txid_env: TARGET_TXID
    state: confirmed
    confirmations: 2
    timeout_secs: 600

  - type: assert_height
    at_least: 205

  - type: assert_component
    component: mining
    reachable: true
    effective_state: running

  - type: faucet
    source: auto
    wait_confirmed: true
    timeout_secs: 900
    outputs:
      - address_env: FUND_ADD_1
        amount: 1btc
      - address: bcrt1q...
        amount: 25000000sat

  - type: checkpoint
    name: mempool_loaded
    timeout_secs: 600

  - type: reorg
    depth: 2
    empty: false
    node: node3
    adds_new_txs: 0
    double_spend_pct: 0

  - type: partition
    node: btc-simnet-node3
    main_blocks: 3
    isolated_blocks: 4

  - type: degrade
    node: node2
    delay_ms: 500
    loss_pct: 1
    seconds: 60

  - type: degrade
    node: node2
    delay_ms: 500
    until_height: 260

  - type: resume_mining
```

Validation rules:

- `wait_height.height` is at least 204.
- `wait_until.timeout_secs` is positive and defaults to 900. Supported conditions are
  `height_at_least` with `height`, `mempool_txs_at_least` with `count`,
  `mempool_txs_at_most` with `count`, and `component`.
- `wait_tx` waits for a user-supplied transaction without indexing or tagging all
  transactions. Use exactly one of `txid` or `txid_env`. Supported states are `seen`,
  `mempool`, `confirmed`, and `missing`; `state` defaults to `confirmed`.
  `confirmations` is only valid with `state: confirmed` and defaults to `1`;
  `timeout_secs` defaults to 900. Quote literal txids in YAML because all-digit hex
  strings can otherwise be parsed as numbers. This is useful for tests such as waiting
  until `TARGET_TXID` has two confirmations, then running an empty reorg deep enough to
  orphan it, then waiting for `state: mempool`.
- `assert_height` requires at least one of `equals`, `at_least`, or `at_most`;
  `equals` cannot be combined with the range fields.
- `assert_component` and `wait_until.kind: component` support `mining`, `spam`,
  `network-agent-node1`, `network-agent-node2`, and `network-agent-node3`. Expectations
  may check `reachable`, `status`, `phase`, `desired_state`, `effective_state`,
  `effective_generation`, `observed_height_at_least`, `active_lease_count`, and
  `cycle_phase`.
- `sleep.secs`, `mine.blocks`, `reorg.depth`, `spam_burst.txs`, and both partition
  block counts are positive.
- Miner nodes are `btc-simnet-node2` or `btc-simnet-node3`.
- `spam_burst.outputs_per_tx` may be zero. Zero sends sequential single-output
  transactions; a positive value sends that many 546-sat burn outputs per
  transaction. Bursts run on a dedicated raw engine (locally signed, submitted with
  `sendrawtransaction`, priced from the live `SPAM_FEE`), so no coin-selection or
  signing load lands on the miner node wallets; the job funds the engine before the
  scenario's steps run, while mining still produces blocks.
- `set_config.settings` is a partial runtime desired-state patch using the same keys as
  `simchainctl config set`. Values may be strings, numbers, booleans, or null/empty reset
  values.
- `assert_config.settings` checks durable desired values, and with `effective: true`
  (the default) also checks that the mining/spam workers expose the expected effective
  policy at the current desired generation.
- `reorg.node` defaults to `node3`. `adds_new_txs` and `double_spend_pct` expose the
  same optional organic/double-spend knobs as `simchainctl reorg start`.
- `faucet.source` defaults to `auto` and also accepts `node2` or `node3`.
  `faucet.outputs` accepts 1 through 100 entries, each with either `address` or
  `address_env` plus an `amount`. Amounts may be decimal BTC (`1`, `0.25`, `1btc`) or
  integer satoshis with a `sat` suffix. `simchainctl scenario` and the standalone
  scenario submitter resolve `address_env` from the client process before upload; raw
  API submissions resolve it in the control plane process. `wait_confirmed: true`
  waits until the transfer confirms before continuing.
- Partition branch lengths differ so the winner is deterministic.
- `degrade.node` is `node1`, `node2`, or `node3`; `delay_ms` or `loss_pct` must be
  positive. `delay_ms` is capped at 600000 and `loss_pct` must be 0 through 100. Use
  exactly one duration: `seconds` from 1 through 86400, or `until_height` at least 204.
- Checkpoint names are non-empty, URL-safe, at most 100 bytes, and unique in one file.
- Checkpoints pause by default. A pausing checkpoint requires a positive `timeout_secs`;
  `pause: false` records a durable milestone and continues immediately.

## Checkpoints and CI

On checkpoint arrival, the server durably records a unique generation and a full live
chain/mining/spam summary before exposing the reached state. A pausing checkpoint then
waits for a matching release, cooperative abort, or its declared timeout. Release is
idempotent for the same generation; stale generations return a conflict.

Use checkpoints when an external test harness or human should decide when the scenario
continues. For example, a scenario can pause at `ready_for_reorg`, let mining and spam
continue, and then run a prewritten reorg only after the caller releases the checkpoint.
Use `wait_tx` when the scenario itself should make that decision from a txid and a target
state or confirmation count. In both flows, the application under test only needs to
broadcast normal regtest transactions; Simchain-specific control stays outside that code.

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

Height waits, manual mining, wallet bursts, and faucet funding use Bitcoin RPC directly.
Runtime config steps use the same validation, worker apply, verification, persistence,
and rollback path as the dashboard and CLI. Mining pause and resume use an expiring
job-owned worker lease. Reorg steps use both mining and spam leases, the reusable reorg
executor, and strict node1 witness convergence. Partition steps lease the namespace-local
target network agent, block P2P ingress and egress, mine both branches, heal, and witness
the deterministic winner before worker leases can resume. Degrade steps lease a target
network agent, apply bounded `netem`, then release it. There is only one public backend:
the control plane.

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
- `tutorial-one-block.yml` pauses background mining, manually mines one block, then resumes.
- `fresh-chain-tour.yml` performs the full hot-control tour after an externally fresh
  chain start: retune, faucet funding, config assertion, empty reorg, organic partition
  reorg, another split, timed degradation, and final fee-floor change.
