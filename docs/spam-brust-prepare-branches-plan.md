# Spam burst branch preparation plan

Status: implemented for reliable on-demand preparation and dashboard shape selection.
The filename intentionally follows the requested `spam-brust` spelling.

The implementation deliberately leaves the Phase 2 capacity telemetry and Phase 3
idle warm reserve as optional follow-up work. Live measurements showed that the bounded
preparation loop is sufficient for correctness: a cold one-branch OUTPUT burst prepared
and completed in about 34 seconds, and a cold ten-branch 20,000-byte DATA burst prepared
and completed in about 69 seconds. Keeping preparation demand-driven avoids permanently
growing the UTXO set merely to optimize demo latency.

## Problem confirmed live

The live stack reported the resident spammer as ready with 45 usable branches per miner,
but this command still failed:

```bash
cargo run -p simchainctl -- spam burst \
  --node node2 --txs 1 --outputs-per-tx 30 --wait
```

The job reached `submitting_spam_burst` and failed with:

```text
raw scenario burst for btc-simnet-node2 is not funded: 0/1 confirmed usable branches
```

This is not contradictory once ownership is considered:

- The resident spammer owns its own raw engines and branch UTXOs.
- Manual and scenario bursts use separate cached `RawSpammer` instances inside the
  control plane, with `scenario-*` key namespaces.
- The separation intentionally prevents the resident worker and the control plane from
  tracking and double-spending the same UTXOs.
- Raising `SPAM_FILL_BLOCK_RATIO` or resident `SPAM_FANOUT_UTXOS` grows the resident
  pool, but does not directly prepare the control-plane burst pool.
- `RawSpammer::ensure_branches` can begin funding/fan-out, but confirmations progress
  between calls. The manual job currently calls it once and immediately turns a
  not-ready state into a terminal failure.

## Recommended outcome

Make every manual burst job own a bounded preparation phase that repeatedly advances
its dedicated engine until enough confirmed branches exist, the request is aborted, or
the existing scenario timeout expires.

Do not share resident-spammer UTXOs and do not solve the problem only by permanently
raising the resident fill ratio. An optional small warm reserve can be added later, but
correct on-demand preparation is required first.

## Dashboard contract

The dashboard should expose both API/CLI burst shapes:

- `Data (OP_RETURN)`: enables `data_bytes` and disables/grays
  `outputs_per_tx`.
- `Outputs`: enables `outputs_per_tx` and disables/grays `data_bytes`.

Send only the active field in the JSON request. Keep the modest default at 10
transactions with 20,000 data bytes; default the inactive output count to 30 for a quick
shape switch. The backend remains authoritative, so CLI, HTTP, MCP, scenario, and
dashboard all receive identical preparation behavior and errors.

## Phase 1: reliable on-demand preparation

### Backend state machine

Refactor the duplicated one-shot preparation in `spam_burst` and
`data_spam_burst` into a helper conceptually like:

```rust
fn wait_for_burst_branches(
    engine: &mut RawSpammer,
    needed: u64,
    deadline: Instant,
    control: &dyn ScenarioControl,
) -> anyhow::Result<BurstPreparation>;
```

Behavior:

1. Select the requested shape and current live spam fee.
2. Compute `needed = min(txs, fanout_utxos).max(1)`.
3. Call `ensure_branches(needed, checkpoint)`.
4. If ready, proceed immediately.
5. If provisioning is pending, keep phase `preparing_spam_burst_branches`, wait about
   500 ms, reconcile/advance again, and repeat.
6. Allow normal mining to confirm funding and fan-out transactions.
7. On abort, stop preparation and perform ordinary owned-lease cleanup.
8. On timeout, return a specific error containing node, shape, usable/needed counts,
   provisioning phase, and the fact that confirmations were awaited.

Do not busy-loop and do not sleep for the whole timeout. The wait must observe abort at
sub-second cadence.

### Lease and coordinator ordering

Keep the mutation coordinator for the complete preparation plus submission. This
prevents a reorg/partition from invalidating funding while the burst job is preparing.

Acquire the resident spam pause lease before the dedicated engine funds itself. Although
the UTXO namespaces differ, both engines may request wallet funding; pausing the resident
worker avoids wallet coin-selection races. Mining must remain running so the preparation
transactions can confirm.

If mining is manually paused or unreachable, fail early with a stable
`burst_preparation_requires_mining` error instead of waiting the full timeout. Do not
silently override an operator's manual pause.

### Shape changes

One dedicated engine per miner is still appropriate. Retargeting from 20,000-byte DATA
to 30-output OUTPUT mode may make previously adequate UTXOs too small, but it should not
discard them. Recalculate `per_tx_required`, count the still-usable UTXOs, and top up only
the deficit.

Branches and key material are recoverable by the existing scenario-specific address
namespace. After a control-plane restart, `reconcile()` should rediscover confirmed
burst UTXOs before deciding to fund more.

## Optional follow-up: truthful capacity and observability

The current dashboard/status `spam_capacity` describes the resident spammer only. Add a
separate burst-capacity view so operators are not told that manual bursts are ready when
only resident branches are ready.

Suggested per-node fields:

```json
{
  "manual_burst_capacity": {
    "node2": {
      "shape": "outputs",
      "shape_value": 30,
      "usable_branches": 10,
      "provisioning": false,
      "last_prepared_at_ms": 0
    }
  }
}
```

Job events/phases:

- `burst_preparation_started`
- `burst_preparation_progress`
- `burst_preparation_ready`
- `burst_preparation_failed`
- phase `preparing_spam_burst_branches`

Rate-limit progress events to capacity changes or a few seconds so JSONL files remain
bounded and readable.

## Optional follow-up: warm reserve

After on-demand preparation is correct, a small idle-time reserve may improve demo
latency. It is an optimization, not the correctness mechanism.

Recommended bounded policy:

- Reserve 10 confirmed branches per miner.
- Size them for the larger of the dashboard defaults: 20,000 DATA bytes or 30 outputs.
- Prepare only after bootstrap, while the mutation coordinator is idle and mining is
  running.
- Suspend immediately when another mutation job starts.
- Reuse the scenario-specific engine namespaces and existing wallet funding path.
- Expose reserve progress separately from resident spam capacity.

Do not tie this reserve to `SPAM_FILL_BLOCK_RATIO`. That setting models persistent
mempool/block load, while manual burst readiness is an operational control-plane
concern. Coupling them caused the misleading assumption that a high ratio prepared the
manual engine.

Before adding settings, prefer internal conservative constants. If real use shows they
need tuning, add narrowly named settings such as:

```text
MANUAL_BURST_RESERVE_BRANCHES=10
MANUAL_BURST_RESERVE_DATA_BYTES=20000
MANUAL_BURST_RESERVE_OUTPUTS_PER_TX=30
```

Do not make an unbounded “lots of branches just in case” pool. Every additional branch
costs a UTXO, wallet funding, fan-out transaction weight, reconciliation work, and more
state to scan after reorgs/restarts. A bounded reserve plus on-demand top-up gives fast
common demos without distorting the simnet.

## Scenario preparation reuse

Scenarios already inspect upcoming `spam_burst` steps and call
`prepare_spam_burst`, including re-preparation after spam-setting changes. Move both
scenario preflight and manual job preparation onto the same wait helper so their
semantics cannot drift.

Scenario preflight should finish before a scenario pauses mining. If a later
`set_config` changes fee or shape-relevant policy, prepare only remaining burst targets,
as the current indexed-target logic intends.

## API and validation

- Preserve the existing request DTO: `txs`, `outputs_per_tx`, optional `data_bytes`.
- Preserve compatibility where `data_bytes` selects DATA mode.
- Consider rejecting simultaneous non-default `outputs_per_tx` and `data_bytes` in a
  later API version. For now the dashboard avoids ambiguity by sending only one.
- Add a reasonable server-side maximum for `outputs_per_tx` before offering an always-hot
  reserve sized from it; the CLI currently has no upper bound.
- Keep `data_bytes <= MAX_DATA_BYTES` and positive `txs` validation.

## Failure and cleanup

- Funding started but not yet confirmed is not corruption. Record the pending phase and
  let a later job/restart reconcile it.
- Aborting preparation releases the spam lease; it does not attempt to abandon or
  double-spend already broadcast funding transactions.
- A reorg during preparation is prevented by coordinator ownership. An external chain
  change is handled by reconcile-and-recount before submission.
- Never claim accepted transactions when only branch funding succeeded.
- Cleanup failure remains separate from preparation/submission failure.

## Implementation sequence

1. Land the dashboard shape selector and mutually disabled inputs.
2. Add a shared, abort-aware branch preparation loop to `RpcScenarioActionBackend`.
3. Use it from manual OUTPUT bursts, manual DATA bursts, and scenario preflight.
4. Add early mining-state validation and stable preparation errors.
5. Evaluate detailed progress callbacks and separate capacity telemetry if demo users
   need more information than the durable `preparing_spam_burst_branches` phase.
6. Evaluate the bounded 10-branch idle reserve only if the measured cold-start latency
   is unacceptable for demos.

## Required tests

- A fresh engine that returns not-ready first and ready after simulated confirmations
  waits and succeeds rather than failing immediately.
- `txs=1, outputs_per_tx=30` succeeds from a cold dedicated engine once one branch is
  confirmed.
- The default `txs=10, data_bytes=20000` succeeds after preparing ten branches.
- Switching DATA to OUTPUT reuses adequate UTXOs and tops up only the deficit.
- Abort and timeout stop promptly and clean the spam lease.
- Manually paused/unreachable mining produces the stable early error.
- Resident and burst engines never share or double-spend a key namespace/UTXO.
- Restart reconciliation discovers already confirmed burst branches.
- Scenario up-front preparation and post-`set_config` re-preparation use the shared loop.
- Dashboard sends only `data_bytes` in DATA mode and only `outputs_per_tx` in OUTPUT mode;
  the inactive input is disabled and visibly dimmed.
- Capacity status clearly distinguishes resident and manual burst engines.

Live acceptance should repeat both commands from a cold control-plane burst cache:

```bash
cargo run -p simchainctl -- spam burst \
  --node node2 --txs 1 --outputs-per-tx 30 --wait

cargo run -p simchainctl -- spam burst \
  --node node2 --txs 10 --data-bytes 20000 --wait
```

Both must reach `succeeded`, report exact accepted counts, and leave no active spam
lease. Then repeat through the dashboard and inspect the mempool transaction shapes.

Run the repository CI-equivalent checks after implementation:

```bash
cargo ba && cargo ca && cargo fac && cargo tt
./scripts/check-compose-security.sh
./scripts/check-docker-images.sh
```
