# Resident spam-fund rebalance: implementation handoff

Status: implementation-ready design  
Scope: resident spammer funds, control-plane job, HTTP API, `simchainctl`, MCP, dashboard, and scenario language

## 1. Goal

Add an explicit, on-demand operation that returns funds held by the resident raw
spammer to the corresponding node wallet used by the faucet.

The operation must:

- work when resident spam is running;
- work when resident spam is already manually paused;
- never let normal spam and rebalance spend the same UTXOs concurrently;
- preserve the user's durable spam state;
- keep mining independent and running unless the user separately paused it;
- be callable through HTTP, `simchainctl`, MCP, the dashboard, and scenarios;
- execute outside the normal spam hot path except for a cheap pending-command check
  at an existing safe point;
- be represented by a normal control-plane mutation job, with events, abort, status,
  persistence, and idempotency behavior consistent with the other mutation jobs.

This is a recovery/maintenance action for disposable simulated chains. It is not an
automatic balance-management subsystem.

## 2. Problem statement

Raw spam transactions generally return change or fan-out outputs to deterministic
spammer-controlled addresses. Fees go to miners, but the unspent outputs remain
owned by the spammer. After a run with very high fees or large funding requests, the
faucet wallet can therefore look nearly empty while a large balance remains in the
resident spammer's branch UTXOs.

Not every coin spent by the spammer is recoverable. OUTPUT-mode spam deliberately
uses `simchain_common::burn_address` destinations whose witness-program hashes do
not correspond to known private keys. Those outputs remain visible on-chain but
are operationally unspendable. Bitcoin can therefore be effectively burned; the
rebalance can recover only outputs controlled by the resident branch/floor keys.
Transaction fees are collected in miner coinbase transactions and become wallet
funds after normal coinbase maturity.

Reducing the spam fee later does not automatically return those UTXOs. The resident
spammer will reuse them, but it has no reason to send the excess back to the faucet
wallet.

The intended solution is a deliberate sweep while spam is cooperatively paused.
This is not a CoinJoin: each key-owned source is swept independently to a fresh
address in its corresponding node wallet. That makes ownership, accounting, and
failure recovery straightforward.

## 3. Decisions and non-goals

### 3.1 Decisions

1. **The control plane owns the public job and pause lease.** It reserves the sole
   mutation slot, acquires/renews a spam lease, starts the worker operation, polls
   it, records events/results, and releases the lease.
2. **The resident spammer performs the sweep.** Its runner already exclusively owns
   the raw-spammer engines, deterministic keys, and in-memory UTXO state. Do not
   duplicate key derivation or signing in the control plane.
3. **The internal worker command is asynchronous.** The spammer's internal
   `tiny_http` server is single-threaded. A synchronous request lasting through
   transaction confirmation would block lease renewal and exceed the internal
   client's timeout.
4. **The durable desired spam state never changes.** A lease creates an effective
   pause without writing `desired_state`.
5. **Version 1 operates on the resident raw spammer only.** It does not sweep
   scenario-burst engines or wallets.
6. **Version 1 sweeps branch funds by default.** The floor-maintenance pool is
   opt-in because it can contain tens of thousands of small UTXOs and require many
   large transactions.
7. **The maintenance fee is fixed at 2 sat/vB.** Never reuse the active spam fee;
   doing so could destroy a large part of the balance during recovery.
8. **Mining is not paused or leased.** The sweep relies on mining to settle current
   spam transactions and confirm sweep transactions.
9. **Amounts in APIs, events, and results are integer satoshis.** Do not use BTC
   floating-point values.
10. **No new dependency or job-store schema version should be necessary.**

### 3.2 Non-goals

- automatic periodic reclaim;
- balance checks on every spam cycle;
- stopping or restarting the spammer process/container;
- redistributing funds between node2 and node3;
- combining inputs controlled by different keys into a CoinJoin-like transaction;
- reclaiming fees already paid to miners;
- reclaiming outputs intentionally sent to `simchain_common::burn_address`
  destinations;
- reclaiming funds from arbitrary historical namespaces;
- sweeping scenario-burst engines in version 1;
- guaranteeing completion while mining is paused and source transactions remain
  unconfirmed.

## 4. Required state behavior

The job uses the existing cooperative lease mechanism. Acquiring a lease must not
change the user's durable desired state.

| State before request | During rebalance | State after success/failure |
|---|---|---|
| desired running, effective running | safe-pause, rebalance | running after lease release |
| desired running, already lease-paused | wait for/acquire compatible ownership, rebalance | follows remaining lease ownership |
| desired paused, effective paused | remain paused, rebalance | remains manually paused |
| spammer unreachable | no transaction is started | job fails clearly; desired state unchanged |
| another mutation job active | no lease or worker command | request receives normal busy conflict |

The critical acceptance case is: **a manually paused spammer can be rebalanced, and
it stays manually paused afterward**.

The worker must remain effectively paused while a rebalance action is in flight,
even if its lease expires. Lease-renewal failure requests cooperative cancellation;
the runner finishes or exits the current atomic broadcast step, reconciles, and only
then allows normal spam to resume.

## 5. End-to-end architecture

```text
HTTP / CLI / MCP / dashboard / scenario step
                    |
                    v
      control-plane shared domain service
                    |
             mutation JobManager
                    |
       acquire + renew spam pause lease
                    |
       POST internal rebalance command
                    |
                    v
        spammer control command queue
                    |
     existing runner safe-point boundary
                    |
        resident node2/node3 engines
                    |
 settle -> scan -> batch -> sign -> broadcast
                    |
       corresponding node wallet addresses
                    |
   poll result, record job events, release lease
```

There is one public domain operation. HTTP, CLI, MCP, dashboard, and scenarios must
all adapt to it; none may implement independent Bitcoin RPC logic.

Keep the repository's existing trust boundary: the control plane talks only to the
authenticated private spammer API and existing domain backends. Do not add a Docker
socket, Docker CLI, process executor, repository bind mount, public worker port, or
new lifecycle mechanism.

## 6. Public request and result contracts

Add shared public types in
`crates/simchain-common/src/control_api/jobs.rs` (or a sibling module re-exported
from the same public API surface).

### 6.1 Request

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpamRebalanceJobRequest {
    #[serde(default)]
    pub include_floor_pool: bool,

    #[serde(default = "default_wait_confirmed")]
    pub wait_confirmed: bool,

    #[serde(default = "default_rebalance_timeout_secs")]
    pub timeout_secs: u64,
}
```

Defaults:

- `include_floor_pool = false`
- `wait_confirmed = true`
- `timeout_secs = 900`

Validation:

- `timeout_secs` must be greater than zero;
- cap it at 86,400 seconds to reject accidental unbounded waits;
- unknown fields are rejected;
- no fee-rate field is exposed in version 1.

Add `JobKind::SpamRebalance` with serialized/display name
`spam_rebalance`. Ensure every exhaustive match over `JobKind` is updated.

### 6.2 Result

The terminal job result should have this logical shape:

```json
{
  "scope": "resident",
  "include_floor_pool": false,
  "wait_confirmed": true,
  "sources": [
    {
      "node": "node2",
      "pool": "branches",
      "input_count": 2208,
      "input_sats": 1232500000000,
      "fee_sats": 305000,
      "returned_sats": 1232499695000,
      "txids": ["..."],
      "confirmed": true
    }
  ],
  "total_input_sats": 1437900000000,
  "total_fee_sats": 650000,
  "total_returned_sats": 1437899350000,
  "remaining_unconfirmed_sats": 0,
  "sweep_transactions": 4
}
```

Use typed shared structs for the worker result and serialize the same shape into the
generic job result. A source is one of:

- node2 / branches;
- node2 / floor;
- node3 / branches;
- node3 / floor.

Omit floor entries when `include_floor_pool` is false. Empty sources are valid and
make the operation a successful no-op.

## 7. Internal spammer command API

Add types to `crates/simchain-common/src/internal_api.rs`:

- `SpamRebalanceRequest`
  - `request_id` (UUID supplied by the control plane);
  - `lease_id`;
  - `lease_owner`;
  - `include_floor_pool`;
  - `wait_confirmed`;
  - `timeout_secs`.
- `SpamRebalanceState`
  - `queued`, `settling`, `sweeping`, `waiting_confirmation`,
    `reconciling`, `succeeded`, `failed`, `cancelled`.
- `SpamRebalanceStatus`
  - request id, state, progress counters, optional result, optional structured error.
- shared result/source structs described above.

Internal routes:

| Method | Route | Behavior |
|---|---|---|
| POST | `/internal/v1/rebalances` | validate active matching lease, enqueue, return immediately |
| GET | `/internal/v1/rebalances/{request_id}` | return state/progress/result |
| POST | `/internal/v1/rebalances/{request_id}/cancel` | request cooperative cancellation |

Use the repository's existing internal authentication and error envelope.

Enqueue rules:

- the referenced lease must exist and its owner must match;
- only one rebalance can be queued or in flight;
- replaying the same `request_id` returns its existing status;
- a different request while one is active returns conflict;
- retain a bounded number of completed statuses, consistent with other bounded
  control-plane/worker histories.

Extend `SpamControlBackend` in `crates/control-plane/src/backend.rs` with start,
status, and cancel methods. Implement them in
`crates/control-plane/src/internal_client.rs` and the test backend in
`crates/control-plane/src/test_support.rs`.

## 8. Spammer control and runner integration

### 8.1 Control state

In `crates/spammer/src/control.rs`, add:

- one optional pending rebalance request;
- one optional in-flight rebalance id;
- cancellation state;
- bounded completed status/result storage;
- enqueue, inspect, progress, complete, fail, and cancel methods;
- a `rebalance_checkpoint(request_id)` method that checks cancellation, shutdown,
  and lease validity between batches/waits.

Extend `SafePointAction` with a rebalance action carrying the request.

The pending rebalance check must occur **after pending policy application but before
the normal `pause_requested` wait branch**. Otherwise a successfully acquired pause
lease would park the runner forever and the queued rebalance could never start.
Enqueue must notify the condition variable.

While the action is in flight:

- do not allow `begin_cycle`;
- report the worker phase as rebalancing;
- keep the effective pause asserted;
- make cancellation observable between settlement polling, batches, and
  confirmation polling;
- never interrupt halfway through signing/broadcasting one transaction.

This adds only a constant-time optional-command check to an existing safe point.
There must be no per-transaction balance query in normal spam operation.

### 8.2 Runner ownership

In `crates/spammer/src/runner.rs`, handle the new safe-point action in the same loop
that owns `SpamEngine::Raw { node2, node3 }`.

The runner should:

1. mark the operation in flight;
2. call an engine-level rebalance method;
3. publish bounded progress after each material transition/batch;
4. reconcile engine state even after partial failure or cancellation;
5. store the typed terminal result/error;
6. clear in-flight state and return to the normal safe-point loop.

If the configured engine does not support raw resident rebalancing, fail before any
chain mutation with a clear `unsupported_engine` error.

## 9. Sweep algorithm

Implement the operation close to
`crates/spammer/src/raw_transaction_spammer.rs` so it can reuse existing key,
wallet, signing, UTXO, and reconciliation helpers.

### 9.1 Source-to-destination mapping

Sweep each source independently:

| Source key/UTXO set | Destination |
|---|---|
| node2 resident branch keys | fresh address from node2 wallet |
| node2 floor-pool key | fresh address from node2 wallet |
| node3 resident branch keys | fresh address from node3 wallet |
| node3 floor-pool key | fresh address from node3 wallet |

Request a fresh wallet address per sweep transaction or per source. Prefer per
transaction if it does not complicate existing wallet RPC helpers. Never direct
node2 funds to node3 or vice versa.

### 9.2 Settle current spam work

The safe pause prevents new cycles but existing raw transactions may still be
unconfirmed. Before selecting sweep inputs:

1. stop initiating new funding/fan-out/fill work;
2. wait for tracked branch tips and pending funding/fan-out transactions to confirm;
3. if floor inclusion was requested, also settle its funding/fan-out and
   `fills_inflight` state;
4. call the existing `reconcile()` so the chain/mempool, not stale memory, is the
   source of truth.

Polling is abort-aware and bounded by the request deadline. Mining is expected to
continue. If mining was separately paused and outstanding source transactions
cannot settle, fail with a timeout that explains that mining must resume before a
retry. Setting `wait_confirmed = false` skips confirmation of the new sweep
transactions; it does not skip this pre-sweep settlement requirement.

Do not sweep an unconfirmed child chain in version 1. Waiting for confirmed,
independently spendable source UTXOs keeps batching and recovery simple.

### 9.3 Input discovery

Reuse the deterministic resident namespace and current raw/floor keys. Use the same
confirmed-address scan and `gettxout(..., include_mempool = true)` filtering used by
`reconcile()`:

- confirmed outputs only;
- exclude outputs already spent in the mempool;
- branch set always included;
- floor set included only when requested.

The mempool-aware check is also the retry/idempotency mechanism: after a partial
run, a retry sees only inputs that remain spendable.

### 9.4 Batching and transaction construction

Use a conservative constant:

```rust
const REBALANCE_MAX_INPUTS: usize = 1_000;
const REBALANCE_FEE_RATE_SAT_VB: u64 = 2;
```

Each transaction has many independent native-P2WPKH inputs and one native-P2WPKH
wallet output. Estimate conservatively:

```text
estimated_vbytes = 12 + (69 * input_count) + 31
fee_sats = estimated_vbytes * 2
output_sats = input_total_sats - fee_sats
```

This keeps a 1,000-input batch well below the standard 100,000-vbyte transaction
limit. If the repository already has an accurate signed-weight calculator, use it
and assert the final fee rate is at least 2 sat/vB.

For every batch:

1. checkpoint cancellation and lease health;
2. sum inputs with checked integer arithmetic;
3. calculate the fee without using the spam policy fee;
4. if the output would be dust or non-positive, leave those inputs unswept and
   report them rather than burning them as fees;
5. obtain the destination address;
6. sign all inputs with the corresponding source key using existing helpers;
7. broadcast through the existing raw-transaction RPC path;
8. record txid, inputs, value, fee, and progress;
9. checkpoint again before starting the next batch.

Passing `maxfeerate=0` to `sendrawtransaction` is acceptable if that is the
repository's existing raw broadcast convention, because the operation independently
enforces its 2 sat/vB fee.

If a 2 sat/vB transaction is temporarily below the node's dynamic mempool minimum,
wait for the paused spam backlog to drain within the same deadline and retry the
broadcast. Do not silently raise the maintenance fee to the active spam fee.

### 9.5 Confirmation and reconciliation

When `wait_confirmed = true`:

- wait for at least one confirmation for every accepted sweep transaction;
- expose confirmation progress;
- use the common request deadline;
- then reconcile all resident engines before reporting success.

When `wait_confirmed = false`:

- success means all intended transactions were accepted to the mempool;
- report them as unconfirmed and populate `remaining_unconfirmed_sats`;
- reconcile using mempool-aware spendability before releasing the lease;
- document that faucet eligibility may remain unchanged until the next block.

After a partial failure or cancellation:

- stop submitting new batches;
- preserve already-broadcast txids in the result/error details;
- reconcile before releasing the effective pause;
- classify the public job as safely aborted/failed, not rolled back.

Broadcast Bitcoin transactions cannot be rolled back by the job system.

### 9.6 Floor-pool cost

The branch-only default is intentional. A representative long run may have only a
few thousand branch UTXOs holding most of the value, while the floor pool can have
roughly 95,000 small UTXOs holding a much smaller balance. At 1,000 inputs per
transaction, floor inclusion can add about 95 sweep transactions.

Expose floor inclusion as an advanced opt-in in every adapter and mention that it
can take substantially longer.

## 10. Control-plane job orchestration

Add `start_spam_rebalance` and `run_spam_rebalance_job` to
`crates/control-plane/src/jobs.rs`.

### 10.1 Reservation

Use `reserve_action_job` with:

- `JobKind::SpamRebalance`;
- the existing sole-mutation conflict rule;
- normal idempotency-key replay behavior;
- serialized validated request in the job record.

Do not add this kind to exceptions allowed while a faucet reservation is armed.
Rebalancing changes the funding wallets and could invalidate faucet capacity
assumptions.

The persisted job schema remains version 2. Adding a `JobKind` variant and generic
request/result does not itself require a schema bump. Confirm that old stored jobs
still deserialize; add a regression fixture if enum serialization makes that
non-obvious.

### 10.2 Execution phases

Use phases/events that make a long operation diagnosable:

1. `acquiring_spam_lease`
2. `waiting_for_spam_safe_point`
3. `starting_rebalance`
4. `settling_spam_inputs`
5. `sweeping_branch_funds`
6. `sweeping_floor_pool` (only when requested)
7. `waiting_for_confirmation` (only when requested)
8. `reconciling_spam_engine`
9. terminal success/failure/abort

Avoid emitting an event per input. Emit per state transition and per transaction
batch so job histories remain bounded.

### 10.3 Lease and polling lifecycle

The job thread:

1. acquires the existing job/scenario-style spam lease and waits for safe pause;
2. starts `OwnedLeaseRenewer`;
3. posts the asynchronous internal command with job id as request id;
4. polls internal status, translating phase/progress to job events;
5. forwards public abort or lease-renewal failure to the worker cancel endpoint;
6. waits for worker terminal/reconciled state;
7. releases the lease in a finally-style cleanup path;
8. completes the public job with the typed result or structured error.

Release with `chain_changed = true` after any sweep transaction was broadcast so
the existing lease release/reconciliation machinery cannot resume spam from stale
tips. It is safe to use the conservative true value when the worker status is
unknown after a transport failure.

### 10.4 Abort and crash behavior

- Abort before any broadcast: terminal `aborted_before_mutation`.
- Abort after one or more broadcasts: stop future batches, reconcile, release the
  lease, and terminal `aborted_safely` with the partial transaction summary.
- Control-plane crash: existing startup recovery marks the job interrupted and
  releases owned worker leases conservatively with chain-changed reconciliation.
  Already-broadcast transactions remain in the mempool/chain.
- Retry after crash: the chain/mempool scan excludes spent inputs, so only remaining
  funds are swept.
- Spammer process crash: public job fails after lease/backend loss. A retry after
  the spammer returns reconciles from chain state.

Do not attempt resumable in-place job execution across a control-plane restart in
version 1.

## 11. Scenario language

Add a step to `crates/scenario-engine/src/schema.rs`:

```rust
RebalanceSpam {
    #[serde(default)]
    include_floor_pool: bool,
    #[serde(default = "default_wait_confirmed")]
    wait_confirmed: bool,
    #[serde(default = "default_rebalance_timeout_secs")]
    timeout_secs: u64,
}
```

Serialized step name: `rebalance_spam`.

Example:

```yaml
version: 1
steps:
  - type: set_config
    settings:
      SPAM_FEE: 0.0001
      SPAM_FILL_BLOCK_RATIO: 1

  - type: rebalance_spam
    include_floor_pool: false
    wait_confirmed: true
    timeout_secs: 900

  - type: faucet
    source: node2
    wait_confirmed: true
    timeout_secs: 900
    outputs:
      - address_env: RECIPIENT_ADDRESS
        amount: 25btc
```

Extend `ScenarioActions` in `crates/scenario-engine/src/engine.rs` and dispatch the
step normally.

### 11.1 Avoid a nested mutation job

A scenario already occupies the control plane's sole mutation slot. Its
`JobScenarioActions` implementation must **not** call public
`start_spam_rebalance`, because that would deadlock or fail as a nested mutation
job.

Extract a shared internal executor used by:

- the standalone rebalance job; and
- `JobScenarioActions::rebalance_spam`.

Scenario behavior:

1. call the existing `ensure_spam_lease(...)`;
2. remember whether this step acquired the lease or reused the scenario's existing
   lease;
3. run the same internal worker command/poll helper;
4. on every exit, release only a lease acquired by this step;
5. preserve a pre-existing scenario lease for later steps;
6. mark reconciliation/chain-changed after any broadcast.

This preserves manual pause state for exactly the same reason as the standalone
job: leases do not modify durable desired state.

### 11.2 Resident-only scope

The step rebalances the resident spammer. It does not sweep dedicated
`spam_burst`/`scenario-{wallet}` engines. Scenario bursts may be prepared or funded
ahead of later steps; draining them would invalidate those prepared targets.

If an all-engine scope is added later, the scenario planner must invalidate and
re-prepare all later burst targets. That work is explicitly deferred.

Update `docs/SCENARIOS.md` with the schema, defaults, resident-only scope, and the
fact that confirmation requires mining.

## 12. Public adapters

### 12.1 HTTP API

Add:

```http
POST /api/v1/jobs/spam-rebalance
Authorization: Bearer ...
Idempotency-Key: optional-client-key
Content-Type: application/json

{
  "include_floor_pool": false,
  "wait_confirmed": true,
  "timeout_secs": 900
}
```

Return the normal `202 JobCreatedResponse`. Reuse standard authentication,
validation, busy-conflict, idempotency, desired-state mutation guard, and error
envelope behavior.

Touch the same layers as other action jobs:

- route and handler in `crates/control-plane/src/api.rs`;
- domain method in `crates/control-plane/src/service.rs`;
- job start/execution in `crates/control-plane/src/jobs.rs`.

### 12.2 `simchainctl`

Under `SpamCommand` add:

```text
simchainctl spam rebalance \
  [--include-floor-pool] \
  [--no-wait-confirmed] \
  [--timeout 900] \
  [--wait] \
  [--idempotency-key KEY] \
  [--json]
```

Semantics:

- `--timeout` populates the request deadline; when `--wait` is used, give the
  client-side watcher a small transport buffer beyond that deadline;
- `--wait` watches the normal job endpoint and prints its terminal result;
- without `--wait`, print the created job;
- `--no-wait-confirmed` maps to `wait_confirmed = false`;
- JSON output uses the unchanged API shape.

Implement the request in `crates/simchainctl/src/client.rs`, arguments in
`crates/simchainctl/src/commands/mod.rs`, and dispatch in
`crates/simchainctl/src/main.rs`.

### 12.3 MCP

Add tool `rebalance_spam_funds` in `crates/control-plane/src/mcp.rs` with:

- `include_floor_pool`, default false;
- `wait_confirmed`, default true;
- `timeout_secs`, default 900;
- optional `idempotency_key`.

The tool returns the created job rather than holding the MCP call open. Mark it as
destructive because it broadcasts transactions, and describe idempotency-key reuse
in its help text. It must call the shared service method.

### 12.4 Dashboard

Add a **Reclaim funds** button beside resident spam pause/resume controls in:

- `crates/control-plane/static/index.html`;
- `crates/control-plane/static/app.js`;
- stylesheet only if the existing action-button styles are insufficient.

Required UX:

- the button remains enabled when spam is manually paused;
- disable it when the spammer is unavailable, its engine is unsupported, or another
  mutation job is active;
- show a confirmation explaining that spam is temporarily/effectively paused,
  mining continues, and a manually paused spammer stays paused;
- default to branch-only;
- expose `include_floor_pool` as an advanced checkbox with a high-UTXO warning;
- generate/reuse an idempotency key for accidental double-click protection;
- submit the public HTTP job and use the existing job banner/watcher;
- on completion show returned BTC (formatted from satoshis), fees, transaction
  count, and whether confirmation is pending.

Suggested confirmation copy:

> Temporarily pauses resident spam and returns its confirmed funds to the node2 and
> node3 faucet wallets. Mining continues. If spam is already paused, it will remain
> paused. Including the floor pool may create many transactions.

## 13. File-by-file implementation checklist

### Shared contracts

- [ ] `crates/simchain-common/src/control_api/jobs.rs`
  - add `JobKind::SpamRebalance`;
  - add public request/result structs and validation/defaults.
- [ ] `crates/simchain-common/src/internal_api.rs`
  - add internal request, state, status, progress, result, and error structs.

### Resident spammer

- [ ] `crates/spammer/src/control.rs`
  - add queued/in-flight/completed rebalance command state;
  - add command priority before the pause wait;
  - add cancellation and progress checkpoints.
- [ ] `crates/spammer/src/runner.rs`
  - handle rebalance safe-point action while owning the engines;
  - guarantee reconciliation and terminal status publication.
- [ ] `crates/spammer/src/raw_transaction_spammer.rs`
  - add settle, discover, batch, sign, broadcast, confirm, and reconcile logic;
  - keep branch and floor sources distinct.
- [ ] `crates/spammer/src/server.rs`
  - add asynchronous internal command/status/cancel routes.

### Control plane

- [ ] `crates/control-plane/src/backend.rs`
  - extend `SpamControlBackend`.
- [ ] `crates/control-plane/src/internal_client.rs`
  - implement internal calls without long synchronous requests.
- [ ] `crates/control-plane/src/test_support.rs`
  - model worker command lifecycle, results, cancellation, and lease checks.
- [ ] `crates/control-plane/src/jobs.rs`
  - reserve/run job, events, polling, abort, cleanup, and shared scenario executor.
- [ ] `crates/control-plane/src/service.rs`
  - expose the shared domain operation.
- [ ] `crates/control-plane/src/api.rs`
  - expose `POST /api/v1/jobs/spam-rebalance`.
- [ ] `crates/control-plane/src/mcp.rs`
  - expose `rebalance_spam_funds`.

### Scenario engine

- [ ] `crates/scenario-engine/src/schema.rs`
  - add and validate `rebalance_spam`.
- [ ] `crates/scenario-engine/src/engine.rs`
  - add action trait method and dispatch.
- [ ] `crates/control-plane/src/jobs.rs`
  - implement the scenario action without creating a nested job.

### CLI and dashboard

- [ ] `crates/simchainctl/src/commands/mod.rs`
  - add `spam rebalance` arguments.
- [ ] `crates/simchainctl/src/client.rs`
  - add public API call.
- [ ] `crates/simchainctl/src/main.rs`
  - dispatch and optionally watch.
- [ ] `crates/control-plane/static/index.html`
  - add action and advanced floor option.
- [ ] `crates/control-plane/static/app.js`
  - state gating, confirmation, submission, and result rendering.

### Documentation

- [ ] `docs/CONTROL_PLANE.md`
  - endpoint, job kind, sole-mutation and lease behavior.
- [ ] `docs/RUNBOOK.md`
  - operator workflow, mining requirement, recovery/retry.
- [ ] `docs/SCENARIOS.md`
  - new step and example.
- [ ] `docs/MCP.md`
  - new tool.
- [ ] `docs/SETTINGS.md`
  - explain that lowering spam fees does not return accumulated UTXOs and link to
    the on-demand operation.
- [ ] `README.md`
  - add the capability and concise CLI example if similar operator actions are
    listed there.

## 14. Tests

### 14.1 Raw-spammer unit tests

- batching at 0, 1, 1,000, and 1,001 inputs;
- conservative vsize and exact checked fee subtraction;
- output cannot be dust/negative;
- branch-only excludes floor inputs;
- node2 and node3 map only to their corresponding wallets;
- raw and floor keys are never mixed in one source batch;
- active spam fee does not affect maintenance fee;
- already mempool-spent inputs are excluded;
- partial broadcast failure leaves unsent inputs available after reconciliation;
- confirmation and broadcast-only results account for satoshis exactly.

### 14.2 Spammer control/runner tests

- enqueue rejects missing or mismatched lease;
- queued command wakes a runner paused on the condition variable;
- rebalance action is selected before the normal pause wait;
- no spam cycle begins while rebalance is in flight;
- desired running resumes after release;
- desired paused remains paused after release;
- duplicate request id is idempotent;
- second request id conflicts while active;
- cancellation is observed between batches;
- lease loss cancels safely without resuming mid-operation;
- completed status history is bounded;
- reconciliation runs after success, failure, and partial cancellation.

### 14.3 Internal API/client tests

- authenticated start/status/cancel round trip;
- start returns promptly and does not consume the 35-second client timeout;
- request validation and structured conflicts;
- test backend supports progress and partial results.

### 14.4 Job/service/API tests

- auth, malformed JSON, unknown fields, and timeout bounds;
- only one mutation job can run;
- faucet-pending state blocks rebalance;
- idempotency-key replay returns the same job;
- lease acquired, renewed, and released;
- running-before returns to running;
- manually-paused-before remains paused;
- no mining lease is acquired;
- worker progress becomes bounded job events;
- abort before mutation and abort after partial broadcast;
- transport failure uses conservative reconciliation cleanup;
- persisted old jobs still load;
- startup recovery releases an owned spam lease.

### 14.5 Scenario tests

- YAML defaults and explicit fields;
- unknown fields and invalid timeout fail validation;
- execution dispatches once with the expected values;
- scenario uses shared executor rather than starting a nested job;
- step acquires/releases a lease when needed;
- step reuses and preserves an existing scenario lease;
- failure/abort cleanup preserves durable manual pause;
- resident-only behavior does not touch prepared scenario-burst engines.

### 14.6 CLI, MCP, and dashboard tests

- CLI method, route, auth, body, idempotency key, `--wait`, and JSON output;
- MCP tool appears in the tool list, validates arguments, and returns a created job;
- dashboard button is enabled while manually paused;
- dashboard button is disabled for active mutation/unavailable/unsupported states;
- double click reuses an idempotency key;
- floor opt-in warning and result formatting.

### 14.7 Live integration test

With a short disposable chain:

1. fund both resident raw engines and generate branch UTXOs;
2. record node2/node3 wallet balances and spammer source balances;
3. run branch-only rebalance while spam is running;
4. verify no conflicting spends, sweep confirmation, wallet increases by
   source-minus-fees, and automatic spam resumption;
5. manually pause spam and run it again;
6. verify it completes and remains manually paused;
7. opt into floor-pool sweep with a small fixture;
8. abort after at least one batch and verify partial accounting plus safe retry;
9. run the scenario step and verify it does not attempt a nested mutation job.

Do not assert exact BTC wallet balance without accounting for mining rewards and
fees. Assert the transaction inputs/outputs and satoshi deltas attributable to the
sweep.

## 15. Observability and errors

Use stable error codes/messages that identify the operator action:

- `spam_rebalance_unsupported_engine`;
- `spam_rebalance_lease_mismatch`;
- `spam_rebalance_already_active`;
- `spam_rebalance_settlement_timeout`;
- `spam_rebalance_mempool_fee_too_low`;
- `spam_rebalance_broadcast_failed`;
- `spam_rebalance_confirmation_timeout`;
- `spam_rebalance_cancelled`;
- `spam_rebalance_worker_unavailable`.

Log request/job id, source, batch number, input count, input satoshis, fee satoshis,
output satoshis, and txid. Never log private keys or raw signing material.

The worker status and job events should be sufficient to distinguish:

- waiting for old spam transactions;
- actively broadcasting sweeps;
- waiting for confirmations;
- reconciling after a partial result.

## 16. Implementation order

Implement in this sequence so each layer can be tested before exposing the next:

1. shared public/internal data contracts and `JobKind`;
2. raw-spammer pure batching/accounting helpers and unit tests;
3. worker control queue, runner action, and async internal routes;
4. control-plane backend/client and mock support;
5. standalone mutation job, service, and HTTP endpoint;
6. scenario schema/action using the shared executor;
7. `simchainctl` and MCP;
8. dashboard;
9. documentation and live integration coverage.

Do not start with the dashboard or add a second backend path; the worker and shared
domain operation are the correctness boundary.

## 17. Verification commands

Run from the repository root:

```bash
cargo fa
cargo ba
cargo ca
cargo fac
cargo tt
./scripts/check-compose-security.sh
./scripts/check-docker-images.sh
```

No dependency change is expected. If a dependency is changed, update and commit
`Cargo.lock` in the same change.

## 18. Definition of done

The work is complete when all of the following hold:

- a user can start a resident rebalance from HTTP, CLI, MCP, dashboard, or a
  `rebalance_spam` scenario step;
- the same domain/job implementation serves every public adapter;
- a running spammer pauses at a safe point and resumes afterward;
- a manually paused spammer rebalances and remains manually paused;
- no normal spam transaction is created while the rebalance action is in flight;
- branch funds return to the corresponding node wallets with transparent fee and
  txid accounting;
- floor-pool sweeping is explicit and off by default;
- mining remains independent;
- abort, timeout, partial broadcast, worker loss, and control-plane restart are
  safely reconciled and retryable;
- the dashboard accurately gates the button and warns about floor-pool cost;
- the scenario implementation does not create a nested mutation job;
- all repository checks pass.
