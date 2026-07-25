# Scenario settings backup and restoration plan

Status: implemented. The v3 private job record captures the full desired baseline in the
same durable reservation that activates the scenario. Normal cleanup and restart
recovery both restore it through the validated apply transaction before releasing the
mutation coordinator.

Live acceptance covered all three terminal paths: a successful scenario restored
generation 0 at generation 2, a deliberately failed assertion restored generation 2 at
generation 4, and force-recreating the control-plane container at a pausing checkpoint
restored generation 4 at generation 6 before clearing the active job.

## Goal

Allow a scenario to make temporary `set_config` changes and reliably restore the
pre-scenario desired configuration when the scenario succeeds, fails, is aborted, or is
interrupted by a control-plane restart.

Today `set_config` calls the ordinary validated apply path. It updates the authoritative
global `state.json`, advances its generation, updates the workers, and intentionally
leaves the new settings in place after the scenario. That behavior must remain the
default for existing version-1 scenarios.

## Proposed scenario contract

Add one optional top-level field:

```yaml
version: 1
restore_settings: true
steps:
  - type: set_config
    settings:
      BLOCK_INTERVAL_MEAN_SECS: 2
      SPAM_FILL_BLOCK_RATIO: 8
  - type: wait_n_blocks
    n: 10
```

Rules:

- `restore_settings` defaults to `false` for backward compatibility.
- `true` means restore on every exit: success, step failure, timeout, cooperative abort,
  panic cleanup, and restart recovery.
- The snapshot covers the complete managed desired-settings map. It does not cover
  chain state, mempool state, wallets, worker pause leases, manual mining/spam desired
  states, environment variables, or boot-only settings.
- A scenario with `restore_settings: true` but no `set_config` step is valid and its
  restore should be an idempotent no-op.
- Do not add `on_success` initially. Leaving experimental settings behind precisely when
  a scenario fails is the least safe behavior and complicates recovery semantics.

## Why a full desired-state snapshot is safe

The scenario owns the single mutation coordinator for its entire lifetime. External
HTTP, MCP, CLI, and dashboard config applies call `ensure_idle()` and are rejected while
the scenario is active. Therefore no legitimate concurrent operator update can be
overwritten by restoring the complete baseline.

If that exclusion rule changes in the future, restoration must move to a touched-key
compare-and-swap design. Do not silently retain full-map restoration after allowing
concurrent config writes.

## Durable backup location

Store the backup inside the private persisted scenario job record:

```text
/var/lib/simchain-control/jobs/index.json
```

In Compose this path is on the `btc-simnet-control-state` named volume. Do not use
`/tmp`, the uploaded YAML, a repository bind mount, worker storage, or a second settings
file. The job record is already atomically written, private (`0600`), retained across
control-plane replacement, and loaded before interrupted-job recovery starts.

Add an internal field to `StoredJob`; do not expose the full baseline through the public
`JobDetail` by default:

```rust
struct ScenarioSettingsBackup {
    baseline_generation: u64,
    baseline_desired: BTreeMap<String, String>,
    captured_at_ms: u64,
    phase: SettingsRestorePhase,
    restored_generation: Option<u64>,
    restored_at_ms: Option<u64>,
    last_error: Option<String>,
}

enum SettingsRestorePhase {
    Captured,
    RestoreRunning,
    Restored,
}
```

The public result/events should report generations and phase, not duplicate the entire
settings map. The private snapshot may remain in the bounded 100-job history for audit
and deterministic retry; managed settings are not secrets and the size is small.

## Capture transaction

The critical invariant is: no scenario setting may change before its backup is durable.

1. Parse and validate the scenario before reserving the coordinator.
2. Acquire the in-process apply mutex and durable `apply.lock` in the existing lock
   order.
3. Confirm that the mutation coordinator is idle.
4. Load the authoritative `ControlState` from `state.json`.
5. Reserve the scenario job and persist `ScenarioSettingsBackup` in the same durable job
   index update that makes the job active.
6. Release the apply locks and start the executor thread.

Avoid capturing only in `run_scenario_job`: the executor thread starts after job
reservation, and a crash in that gap must still leave enough state for restart recovery.
Idempotency-key reuse must return the original job and its original snapshot; it must
never take a new baseline.

This likely requires a scenario-specific reservation method or extending
`reserve_action_job` with optional recovery material. Keep generic jobs free of scenario
schema knowledge.

## Normal restore lifecycle

Restoration is cleanup owned by the scenario job, not an extra scenario step:

1. Execute steps normally.
2. Stop the lease renewer.
3. Heal network impairments and release worker leases using existing cleanup ordering.
   Policy restoration must not race an active worker lease that rejects policy changes.
4. Set job phase to `restoring_settings` and persist backup phase `RestoreRunning`.
5. Reload current durable control state and apply the complete baseline through
   `apply_with_context`; never overwrite `state.json` directly.
6. Use the current generation as `base_generation`. This gives a final compare-and-swap
   check while preserving normal validation, worker apply, rollback, and atomic save
   behavior.
7. Persist `Restored`, the resulting generation, and timestamps.
8. Only then make the job terminal and release the mutation coordinator.

The restore is idempotent. If current desired state already equals the baseline and both
workers expose it effectively, treat the operation as restored without forcing a new
generation. Re-running recovery after a crash is therefore safe.

## Restart recovery

Extend `recover_job_resources` for interrupted scenario jobs:

1. Recover network resources and any reorg recovery material.
2. Release owned spam/mining leases and verify they are gone.
3. If a settings backup exists and is not `Restored`, restore it through the same
   validated apply path.
4. Keep `active_job_id` set and cleanup state `running` while restoration is pending.
5. Reuse the existing two-second recovery retry loop for transient worker/RPC failures.
6. Clear `active_job_id` only after resource recovery and settings restoration both
   succeed.

If the process died after the apply succeeded but before the job record said `Restored`,
the next retry observes an already-matching desired/effective state and completes as a
no-op.

## Failure semantics

- A scenario whose steps succeeded but whose settings restore failed is not a clean
  success. Report the primary scenario result plus `cleanup.state = failed/running` and
  a `settings_restore_failed` cleanup error.
- Transient restart-recovery failures keep the job active and retry, matching network
  impairment recovery behavior.
- A validation failure while restoring a previously valid canonical snapshot indicates
  an incompatible software upgrade or corrupt state. Keep the snapshot, keep the error
  visible, and do not write state directly to bypass validation.
- Restoration must not undo chain mutations. It restores configuration only.
- Aborting during `restoring_settings` must not cancel restoration; abort controls
  scenario work, not mandatory cleanup.

## Persistence migration

The job index is currently schema version 2. Add the optional backup field with a
serde default and bump the job schema to version 3 so the durable format change is
explicit.

- Add `PersistedJobsV2`/`StoredJobV2` matching the current format.
- Migrate v1 to v3 and v2 to v3 with `scenario_settings_backup: None`.
- Pin migrations with fixtures for an inactive job and an interrupted active scenario.
- Do not infer a baseline for an old interrupted scenario; it never opted into restore.

## Events and user-visible state

Emit stable job events:

- `settings_snapshot_captured`
- `settings_restore_started`
- `settings_restored`
- `settings_restore_pending`

Include `baseline_generation`, current/restored generation, and changed-key count. Do
not put the entire baseline in event JSONL. Add `restoring_settings` to the phases shown
by jobs watch/dashboard. Document that config mutation remains blocked through restore.

## Implementation sequence

1. Add and validate `Scenario.restore_settings` in `scenario-engine` with default false.
2. Add v3 job persistence types, migration, and `ScenarioSettingsBackup`.
3. Make scenario reservation capture baseline state atomically with job activation.
4. Add an idempotent `restore_scenario_settings(job_id)` helper using
   `apply_with_context`.
5. Call it after owned-resource cleanup and before terminal job completion.
6. Integrate it into interrupted-job recovery before `active_job_id` is cleared.
7. Add events, phases, public cleanup errors, and documentation examples.

## Required tests

- Missing field preserves current persistent-setting behavior.
- `restore_settings: true` restores after success, validation failure, checkpoint
  timeout, abort, and a panicking executor.
- Multiple `set_config` steps restore the exact original full map.
- A no-op scenario does not unnecessarily advance generation.
- Idempotency reuse keeps the original baseline.
- External config apply is rejected while the scenario and its restore are active.
- Restart before step 1, during `set_config`, after the last step, during restore, and
  after apply/before backup-phase persistence all converge to the baseline.
- Worker apply failure is visible and recovery retries without releasing the mutation
  coordinator.
- v1/v2 job stores migrate without inventing restore material.
- The snapshot and job index retain private file permissions.

Run the repository CI-equivalent checks after implementation:

```bash
cargo ba && cargo ca && cargo fac && cargo tt
./scripts/check-compose-security.sh
./scripts/check-docker-images.sh
```
