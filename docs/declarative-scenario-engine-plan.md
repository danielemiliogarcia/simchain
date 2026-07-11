# Implementation plan: declarative scenario engine

## Status: READY TO IMPLEMENT (written 2026-07-10)

Implements nice-to-have **"1. Declarative scenario engine"** from
[NICE-TO-HAVE.md](NICE-TO-HAVE.md). That entry gives the product rationale; this document
is the engineering hand-off: exact scope, architecture, schema, file-level changes, and
the verification plan.

## 1. Goal and non-goals

**Goal:** ship an opt-in orchestration container that reads a `scenario.yml` and executes
an ordered sequence of chain actions reproducibly against a running simnet.

Primary command:

```bash
SCENARIO_FILE=scenarios/reorg-during-sync.yml docker compose --profile scenario up btc-simnet-scenario
```

v1 should support these step types:

- `wait_height`
- `sleep`
- `pause_mining`
- `resume_mining`
- `mine`
- `reorg`
- `spam_burst`
- `partition`

The engine should:

- wait for a safe starting state,
- execute steps in order,
- stop on the first failure,
- log step start/end clearly,
- exit `0` on success and non-zero on failure so downstream CI can use it directly.

**Non-goals:**

- No pre-bootstrap history mutation in v1. The bootstrap sequence is height-coded and not
  safe to interleave arbitrarily with scripted mining or partitions before height 204.
- No generic workflow DSL, loops, branches, variables, or templating. Ordered steps only.
- No embedded assertions in v1 (`assert_height`, `assert_tx`, etc.). The scenario engine
  is the history generator; downstream tests keep their own domain assertions.
- No web UI.
- No refactor of the mining controller or reorg simulator into a shared library just so
  the scenario engine can call them in-process. v1 should reuse the existing CLI/script
  surfaces wherever practical.

## 2. Current state (what the plan builds on)

- **The simnet already has the right primitive actions, but only manually.**
  Users can already:
  - stop/start the mining controller,
  - run one-shot reorgs with `./scripts/simulate-reorg.sh`,
  - send manual wallet txs from the runbook,
  - and, after feature #2, run partition/netem helpers.
- **The mining controller is resumable.**
  `crates/mining-controller/src/bootstrap.rs` is stage-resumable by height, and the
  controller restarts cleanly after `docker compose stop/start`. That means "pause
  mining" can be implemented operationally by stopping the controller container instead
  of inventing a new control protocol.
- **The spammer is also operationally controllable.**
  It is stateless between cycles and reacts to new blocks, so one-shot burst tooling can
  live outside it without corrupting its internal state.
- **The reorg simulator already exists as a dedicated tool.** It has a stable CLI wrapper
  (`scripts/simulate-reorg.sh`) and should be reused, not duplicated.
- **The partition feature should expose script helpers.** The scenario engine should reuse
  the exact `partition.sh` / `netem.sh` control surfaces from feature #2, not fork its
  own partition implementation.
- **The repo has no general orchestration container today.** The dashboard plan already
  identified the pattern for a repo-mounted, `docker.sock`-powered helper container; the
  scenario engine can use the same operational model without exposing any HTTP port.

## 3. Scope choice: v1 starts after bootstrap

This is the most important scope decision in the plan.

The nice-to-have sketch gives examples like "at height 150 reorg 2 blocks". With the
current bootstrap design, v1 should **not** try to support that.

Reason:

- bootstrap stages are keyed to fixed target heights,
- mining or rewriting history before height 204 can skip intended funding stages or
  rewrite the funding blocks themselves,
- turning bootstrap into a scenario-aware state machine would be a large feature on its
  own.

So v1 semantics are:

- the scenario engine waits until `node1` height `>= 204`,
- then it begins executing the declared steps,
- if users need a more specific starting state, they can restore a snapshot first.

This keeps the engine practical and reproducible without entangling it with bootstrap.

## 4. User-visible v1 behavior

Example scenario:

```yaml
version: 1
steps:
  - type: wait_height
    height: 260

  - type: pause_mining

  - type: spam_burst
    node: btc-simnet-node2
    txs: 500
    outputs_per_tx: 25

  - type: reorg
    depth: 2

  - type: partition
    node: btc-simnet-node3
    main_blocks: 3
    isolated_blocks: 4

  - type: resume_mining
```

The engine should log:

- scenario file path,
- bootstrap wait start/end,
- each step index, type, and parameters,
- step duration,
- final height and best block hash,
- success or the first failing step.

On success it exits. It does **not** stay resident.

## 5. Architecture overview

Implement the engine as a new workspace binary:

- crate: `crates/scenario-engine`
- package name: `simchain-scenario-engine`
- compose service name: `btc-simnet-scenario`
- compose profile: `scenario`

Runtime model:

1. The container mounts the repo root at `/workspace`.
2. It also mounts `/var/run/docker.sock`.
3. It talks to node1/node2/node3 over the control network via normal RPC.
4. For actions that already exist as scripts/tools, it shells out against the mounted
   repo:
   - `scripts/simulate-reorg.sh`
   - `scripts/partition.sh`
   - `scripts/netem.sh` if needed later
5. For simple one-shot actions (`mine`, `spam_burst`) it uses RPC directly.

This is intentionally orchestration-heavy and refactor-light. The feature is supposed to
turn simchain into a harness, not to rewrite every tool as a library first.

## 6. Change 1: add a new `crates/scenario-engine` binary

Create:

```text
crates/scenario-engine/
  Cargo.toml
  src/
    main.rs
    config.rs
    schema.rs
    engine.rs
    steps.rs
    docker.rs
    rpc.rs
    burst.rs
    results.rs
```

Suggested responsibilities:

- `config.rs`
  - parse env like `SCENARIO_FILE`, `SIMCHAIN_REPO_ROOT`
- `schema.rs`
  - `serde` structs/enums for the YAML schema
- `engine.rs`
  - the ordered step interpreter
- `steps.rs`
  - implementation of each supported step type
- `docker.rs`
  - `docker compose` / script invocation wrappers
- `rpc.rs`
  - node client helpers and height polling
- `burst.rs`
  - one-shot wallet burst implementation
- `results.rs`
  - final run summary struct and optional JSON output

### Dependencies

Expected dependencies:

- `serde`
- `serde_yaml`
- `serde_json`
- `anyhow`
- `tracing`
- `dotenvy`
- `bitcoincore-rpc`
- `simchain-common`

No async runtime is required. The engine is a single sequential interpreter and can stay
fully synchronous.

## 7. Change 2: add a scenario compose service and image target

### Workspace

Update the root `Cargo.toml` workspace members to include `crates/scenario-engine`.

### Docker build

Extend `docker/tools.Dockerfile` with a `scenario-engine` final target. Like the dashboard
panel, this final stage needs:

- the Rust binary,
- Docker CLI,
- compose plugin,
- CA certs.

Use a Debian-based final stage so the glibc-linked binary runs cleanly.

### Compose service

Add to `docker-compose.yml`:

- service: `btc-simnet-scenario`
- profile: `scenario`
- build target: `scenario-engine`
- repo bind mount: `.:/workspace`
- Docker socket mount: `/var/run/docker.sock:/var/run/docker.sock`
- environment:
  - `SIMCHAIN_REPO_ROOT=/workspace`
  - `SCENARIO_FILE=${SCENARIO_FILE:-/workspace/scenarios/example.yml}`
- network: `btc-simnet-control`
- depends_on:
  - `btc-simnet-node1` healthy
  - `btc-simnet-node2` healthy
  - `btc-simnet-node3` healthy

Do **not** add it to `all-tools`. Like the panel, it holds `docker.sock` and is therefore
an explicit opt-in operational tool.

## 8. Change 3: define the v1 YAML schema

Keep the schema deliberately small. Ordered steps are enough.

### Top level

```yaml
version: 1
steps:
  - ...
```

Reject any version other than `1`.

### Step enum

Use a tagged enum:

```yaml
- type: wait_height
  height: 260

- type: sleep
  secs: 30

- type: pause_mining

- type: resume_mining

- type: mine
  node: btc-simnet-node2
  blocks: 3

- type: reorg
  depth: 2
  empty: false

- type: spam_burst
  node: btc-simnet-node2
  txs: 500
  outputs_per_tx: 25

- type: partition
  node: btc-simnet-node3
  main_blocks: 3
  isolated_blocks: 4
```

### Validation rules

- `wait_height.height >= 204`
- `sleep.secs > 0`
- `mine.blocks > 0`
- `reorg.depth > 0`
- `spam_burst.txs > 0`
- `spam_burst.outputs_per_tx >= 0`
- `partition.node` in `{btc-simnet-node2, btc-simnet-node3}`
- `partition.main_blocks > 0`
- `partition.isolated_blocks > 0`
- `partition.main_blocks != partition.isolated_blocks`

No implicit defaults beyond trivial booleans. The scenario file should be explicit.

## 9. Change 4: implement step semantics

### Engine lifecycle

On startup:

1. load `.env`,
2. parse the scenario file,
3. build node clients,
4. wait for `node1` RPC,
5. wait until `node1` height `>= 204`,
6. begin step execution.

### `wait_height`

- Poll node1 height every 500 ms.
- Log start and finish heights.
- Add a global default timeout (for example 30 minutes), overridable later if needed.

### `sleep`

- Plain wall-clock sleep.
- Log seconds and start/end timestamps.

### `pause_mining`

Implement by:

```bash
docker compose stop btc-simnet-mining-controller
```

Use a compose wrapper in `docker.rs`, not raw string concatenation in the step handler.

### `resume_mining`

Implement by:

```bash
docker compose up -d btc-simnet-mining-controller
```

Then wait for the container to be running again.

### `mine`

Implement directly over RPC:

- choose the requested miner node (`node2` or `node3`),
- resolve its configured wallet,
- get a fresh wallet address,
- call `generatetoaddress(blocks, address)`.

Direct RPC is simpler and more deterministic here than shelling out to `bitcoin-cli`.

### `reorg`

Reuse the existing helper surface:

```bash
./scripts/simulate-reorg.sh <depth> [empty]
```

The engine should invoke the script through `docker.rs`, capture stdout/stderr, and fail
the step if the command exits non-zero.

This keeps the scenario engine aligned with the standalone reorg tool, including any
future additions like double-spend parameters.

### `spam_burst`

Implement as a one-shot wallet burst in the engine itself.

Behavior:

- select the requested miner wallet (`node2` or `node3`),
- use `sendtoaddress` in sequential mode when `outputs_per_tx == 0`,
- use `sendmany` batches to burn addresses when `outputs_per_tx > 0`,
- default amount per output to the existing spam dust shape (546 sats),
- use replaceable txs only if/when an explicit flag is added later.

Why in-engine instead of reusing the spammer binary:

- the spammer is an infinite block-triggered process, not a one-shot CLI,
- adding a simple deterministic wallet burst here is much smaller than refactoring the
  whole spammer into a reusable library.

### `partition`

Do **not** reimplement partition logic in the scenario engine. Invoke:

```bash
./scripts/partition.sh run <node> --main-blocks N --isolated-blocks M
```

That keeps the partition feature as a single implementation reused both manually and by
scenarios.

## 10. Change 5: add `scripts/spam-burst.sh` as a reusable manual helper

Although the engine can implement bursts directly, a standalone helper script is still
worth adding because:

- it gives users the same one-shot primitive manually,
- it keeps the burst behavior easy to test outside the scenario engine,
- it mirrors the existing `simulate-reorg.sh` / `partition.sh` operator pattern.

Suggested surface:

```text
spam-burst.sh <miner-node> --txs N [--outputs-per-tx M]
```

The scenario engine may call this script instead of its in-process implementation if that
proves cleaner during implementation. The key point is to have exactly one burst behavior,
not two divergent ones.

## 11. Change 6: step failure handling and cleanup policy

The engine should stop on the first failed step. But it also needs a small amount of
best-effort cleanup so it does not leave the simnet in an accidental half-managed state.

Track engine-owned state:

- whether the engine stopped the mining controller,
- whether it stopped the spammer indirectly via a partition run,
- whether the current step is a partition run in progress.

Cleanup policy:

- if the engine itself paused mining and the scenario fails later, restart the mining
  controller before exit unless the failure happened inside `resume_mining`,
- if a partition helper fails after disconnecting a node, call `partition.sh heal ...`
  best-effort before exit,
- log cleanup failures separately but preserve the original failing step as the exit
  reason.

This is enough to prevent the most annoying operator outcome: "scenario failed and left
the network split with mining disabled."

## 12. Change 7: add example scenarios

Create:

```text
scenarios/
  reorg-during-sync.yml
  pause-then-burst.yml
  partition-node3.yml
```

Recommended examples:

1. **`pause-then-burst.yml`**
   - wait for a post-bootstrap height,
   - pause mining,
   - send a large wallet burst,
   - resume mining.

2. **`reorg-during-sync.yml`**
   - wait for height,
   - trigger a one-shot reorg,
   - continue.

3. **`partition-node3.yml`**
   - wait for height,
   - run a deterministic partition where node3 wins by one block,
   - resume background mining if desired.

These become the executable examples for docs, bug reports, and downstream CI.

## 13. Why stopping/starting the mining controller is the right v1 control surface

The original sketch suggested either replacing the controller or adding a control file/HTTP
endpoint. For this repo, v1 should prefer operational pause/resume by container lifecycle.

Reasons:

- no changes to controller logic,
- bootstrap already resumes by height,
- stopping the controller is exactly what users already do manually when they want manual
  mining,
- it matches the partition feature, which must stop the controller anyway.

Adding an in-process control plane to the controller is still possible later, but it is
not required to land the declarative harness.

## 14. What needs no behavior changes

- **Mining controller internals:** no code changes required in v1.
- **Reorg simulator internals:** no code changes required; it is reused as-is.
- **Spammer internals:** no code changes required for the base scenario feature.
- **Snapshot machinery:** unchanged; users can restore a snapshot first, then run a
  scenario against that known starting state.

## 15. Documentation updates (same PR)

- **README.md**
  - add the `scenario` profile to the profile table,
  - add a short "Scenarios" section with the command line and pointer to scenario docs.
- **docs/SETTINGS.md**
  - add `SCENARIO_FILE` and any optional result-path env.
- **docs/RUNBOOK.md**
  - add one-shot scenario invocation examples.
- **docs/SCENARIOS.md**
  - new user-facing schema and examples document.
- **docs/NICE-TO-HAVE.md**
  - remove item #1 once shipped and renumber.

## 16. Verification plan

### Automated

Add unit tests for:

1. schema parsing:
   - valid v1 file,
   - unknown version,
   - invalid step fields,
   - equal partition block counts rejected.
2. ordered execution planning:
   - steps stay in file order,
   - the correct wrapper command is built for reorg and partition steps.
3. cleanup policy:
   - failed scenario after `pause_mining` schedules `resume_mining`,
   - failed partition run schedules best-effort heal.

Mock `docker.rs` command execution behind a trait so tests do not need Docker.

### Manual, in order

1. **Baseline startup**
   - `docker compose --profile scenario up -d --build`
   - confirm the engine waits for bootstrap and then executes the selected scenario.

2. **Pause/resume**
   - run `pause-then-burst.yml`,
   - confirm mining controller stops, the burst lands, then controller restarts and the
     chain continues.

3. **Reorg scenario**
   - run `reorg-during-sync.yml`,
   - confirm the reorg helper executes and the scenario exits success.

4. **Partition scenario**
   - run `partition-node3.yml`,
   - confirm it reuses the partition helper and the chain converges on the expected
     winner.

5. **Failure path**
   - intentionally break one step (for example bad partition node name),
   - confirm the engine exits non-zero and restarts mining if it had paused it.

6. **Result artifact (if implemented)**
   - confirm the final JSON summary includes executed-step count, final height, and
     success/failure.

## 17. Risks and edge cases

- **Pre-bootstrap scenarios are explicitly unsupported in v1.** This is the main tradeoff
  and must be documented clearly.
- **Stopping the mining controller resets its in-memory chain view.** That is acceptable;
  on restart it reseeds from the live chain. The scenario engine should not promise
  uninterrupted controller log continuity.
- **`spam_burst` is intentionally simpler than the continuous spammer.** It is a
  deterministic one-shot traffic generator, not a full reuse of raw-engine internals.
- **The engine holds `docker.sock`.** Like the dashboard panel, it must remain an opt-in
  helper profile, not part of `all-tools`.
- **Helper-script reuse means command surfaces become stability boundaries.** The scripts
  the scenario engine calls should be treated as part of the repo's supported operator
  interface once this lands.

## 18. Effort and change list

Large, but mostly orchestration glue rather than core-protocol code.

| File | Change |
| --- | --- |
| `Cargo.toml` | Add `crates/scenario-engine` to the workspace |
| `docker/tools.Dockerfile` | Add `scenario-engine` final target with Docker CLI + compose plugin |
| `docker-compose.yml` | Add `btc-simnet-scenario` service/profile with repo mount + `docker.sock` |
| `crates/scenario-engine/**` | New scenario interpreter binary |
| `scripts/spam-burst.sh` | New one-shot burst helper (manual + scenario reuse) |
| `scenarios/*.yml` | Example scenarios shipped with the repo |
| `README.md` | Document the `scenario` profile and CLI |
| `docs/SCENARIOS.md` | New schema/examples doc |
| `docs/SETTINGS.md` | Document `SCENARIO_FILE` and related env |
| `docs/RUNBOOK.md` | Add scenario invocation recipes |
| `docs/NICE-TO-HAVE.md` | Remove item #1 once implemented |

## 19. Recommended implementation order

1. Add the new crate, YAML schema parser, and dry-run logging.
2. Add the compose service and Docker image target.
3. Implement `pause_mining`, `resume_mining`, `wait_height`, `sleep`, and `mine`.
4. Add `reorg` step reuse through `simulate-reorg.sh`.
5. Add `spam_burst`.
6. Add `partition` step reuse once feature #2 is in place.
7. Ship example scenarios and docs.

That order lands value incrementally while keeping the highest-risk external integrations
for later steps.
