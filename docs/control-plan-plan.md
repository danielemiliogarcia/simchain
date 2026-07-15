# Implementation plan: Simchain control plane

## Status

**READY TO IMPLEMENT** — written 2026-07-15.

This document is the engineering handoff for evolving the implemented dashboard and
declarative scenario engine into one socket-free Simchain control plane. It pins the
target architecture, ownership boundaries, public API, CLI, scenario/CI protocol,
dashboard integration, migration sequence, file-level changes, failure behavior, and
verification criteria.

The implementation branch is `control-plane`, created from dashboard commit `6ef29e5`.
The dashboard branch is itself a direct descendant of scenario commit `4ff9bca`, so the
control-plane work starts with both features and does not need to reconcile divergent
histories.

## 1. Decision summary

Evolve `simchain-panel` into the Simchain control plane. Do **not** add a second backend
beside the panel, and do **not** turn every existing binary into a public microservice.

The final design has these properties:

- One localhost public server exposes the browser dashboard, versioned HTTP/JSON API,
  and MCP endpoint.
- A first-party `simchainctl` CLI is the primary terminal and CI client. Shell scripts
  become compatibility wrappers around the CLI/API rather than Docker orchestrators.
- Docker Compose starts stable processes, supplies networks and volumes, health-checks
  them, and restarts crashed processes. It is no longer the application control
  protocol.
- The control plane owns orchestration: desired runtime configuration, one active
  chain-mutating job, pause leases, scenario execution, progress events, and results.
- The mining controller and spammer remain separate long-running processes and expose
  narrow internal control APIs. They own their mutable engine state and acknowledge
  commands only at safe points.
- Reorgs and scenarios are bounded control-plane jobs, not idle public services and not
  newly created Docker containers.
- Per-node network agents own latency, loss, and hard P2P isolation inside the node
  network namespaces. They have narrowly scoped `NET_ADMIN`, never Docker socket
  access.
- No Simchain container mounts `/var/run/docker.sock` in the completed architecture.
- The control plane is not in the chain's critical data path: if it is unavailable,
  bitcoind, mining, and spam continue with their last effective state.

The guiding ownership rule is:

```text
Docker                 process lifecycle
Bitcoin Core nodes     chain, mempool, wallets, P2P state
Mining/spam workers    local engine state and safe execution points
Network agents         node-namespace network impairment state
Control plane          desired behavior, coordination, jobs, API and observability
Clients                user/CI intent only; never direct container lifecycle control
```

## 2. Product goals

### Human-operated simulation

A user should be able to open one local dashboard and:

- see height, recent blocks, cadence, mempool depth, fee distribution, and component
  health;
- see exactly what operation is active and which phase or scenario step it has reached;
- pause/resume mining or spam without stopping containers;
- change block cadence, miner weights, spam fill ratio, fee/fanout policy, and other
  supported runtime settings without recreating containers;
- request manual blocks, spam bursts, reorgs, partitions, and declarative scenarios;
- see live progress, failures, cleanup, and recent job results;
- follow the effect in the local mempool.space explorer through a prominent health/link
  integration rather than reimplementing an explorer inside the dashboard.

### CI and downstream-system testing

A CI job should be able to:

1. start Simchain and wait for control-plane readiness;
2. submit a scenario document without bind-mounting it into a container;
3. wait until the scenario reaches a named checkpoint;
4. run external tests against Bitcoin RPC, Electrum, mempool.space, or the downstream
   system under test while the scenario is held at that state;
5. release the checkpoint and let the scenario continue;
6. wait for a terminal result and save a machine-readable artifact;
7. receive a non-zero CLI exit code on scenario failure, timeout, interrupted execution,
   or failed cleanup.

The scenario engine remains a history/environment generator. External tests retain
their domain assertions; the YAML does not grow into a general test language.

### Programmatic operation

Every user-visible operation must have one domain implementation reachable through:

- HTTP/JSON for integrations and debugging;
- `simchainctl` for humans, scripts, and CI;
- MCP tools for coding agents;
- the dashboard for interactive operation.

The transports are adapters over the same service layer. They must not duplicate
validation, locking, cleanup, or execution logic.

## 3. Non-goals

- No generic Docker, host, or Kubernetes management API.
- No Docker socket proxy. Restricting a proxy still leaves lifecycle and network
  semantics coupled to Docker and does not produce application-safe pause points.
- No message broker, service discovery system, distributed database, or workflow
  platform.
- No public API per existing binary. Worker APIs are private implementation details on
  `btc-simnet-control`.
- No remote/multi-user control plane or production authentication system in v1. The
  public listener stays loopback-only with bearer-token protection on mutations.
- No replacement for mempool.space. The dashboard supplies simulation controls and
  operational telemetry; mempool.space supplies explorer-grade transaction/block views.
- No promise that every environment variable becomes hot-reloadable. Settings are
  explicitly classified as runtime, safe-point reconfiguration, or boot-only.
- No node image, datadir, RPC credential, host-port, consensus, relay-policy, or
  capacity-policy mutation through the control plane in v1.
- No pre-bootstrap scenario history mutation. Existing height-204 bootstrap safety
  remains in force.
- No general scenario loops, arbitrary expressions, branching, templating, or embedded
  application assertions.

## 4. Existing code to build on

### Dashboard branch

The dashboard is already an API-first server, not merely an HTML page:

- `crates/panel/src/api.rs` — Axum routes, static UI, bearer-token guard, and loopback
  Host validation.
- `crates/panel/src/service.rs` — transport-independent operations and shared JSON/error
  types used by HTTP and MCP.
- `crates/panel/src/mcp.rs` — MCP tools over the same service functions.
- `crates/panel/src/status.rs` — cached node1 status, recent blocks, cadence, mempool,
  fee histogram, and current Docker-derived service state.
- `crates/panel/static/*` — schema-driven dashboard that already calls `/api/v1` rather
  than reaching backend internals.
- `crates/simchain-common/src/live_tuning.rs` — source-independent mining/spam setting
  catalog, parsing, canonicalization, and validation shared with the worker binaries.
- `crates/panel/src/apply.rs` — a carefully tested but transitional
  validate/write/recreate/inspect/rollback implementation.
- `crates/panel/src/compose.rs`, `docker_inspect.rs`, and `envfile.rs` — Docker and host
  file adapters required by the current apply implementation.

The API/security/status/schema patterns are retained. The Docker-backed apply path is
replaced rather than wrapped in another API.

### Scenario branch

- `crates/scenario-engine/src/schema.rs` — strict ordered YAML schema and validation.
- `engine.rs` — ordered execution, step logging, first-failure behavior, and cleanup
  ownership tracking.
- `results.rs` — machine-readable terminal summary.
- `steps.rs` — direct RPC implementations for wait, mine, and spam burst.
- `docker.rs` — transitional Docker/script backend for pause/resume, reorg, and
  partition. This module must not survive in the final scenario execution path.

### Long-running workers

- Mining currently captures a static `OnceLock` configuration and uses blocking sleep.
  It needs an interruptible control channel and a versioned runtime policy.
- The raw spammer owns mutable keys/UTXO branches and floor-pool state. Restart recovery
  exists, but cooperative pause/reconfigure/reconcile is safer and faster.
- The existing raw-spam reorg plan already defines a sound lease protocol: idempotent
  acquisition, safe-point acknowledgement, heartbeat/TTL, release with
  `chain_changed`, and reconciliation before resume. Generalize that design instead of
  creating a second pause mechanism.

### Bounded operations and network tooling

- The reorg crate talks Bitcoin RPC and does not intrinsically require Docker. Docker is
  currently only its process launcher.
- `partition.sh` genuinely changes Docker network attachment and coordinates worker
  stop/start. Its domain workflow moves into the control plane.
- Existing netem helpers already join node network namespaces with `NET_ADMIN`. They are
  the migration base for persistent, narrow network agents.

## 5. Target topology

```text
                           localhost:8090
          +------------------------------------------------+
          |         simchain-control-plane                 |
Browser --|  static UI -> /api/v1                          |
CLI ------|  HTTP API, MCP, auth                           |
MCP ------|  status cache, desired config                  |
          |  mutation coordinator, jobs, events/results    |
          |  scenario + reorg executors                    |
          +-----+------------------+-------------------+----+
                |                  |                   |
        internal HTTP       internal HTTP         Bitcoin RPC
                |                  |                   |
       mining-controller        spammer          node1/2/3
                |                  |
                +------- pause/config leases ----------+

          internal HTTP to one network agent per node
                |              |              |
          node1 netns      node2 netns      node3 netns
          NET_ADMIN        NET_ADMIN        NET_ADMIN

Docker Compose creates and supervises the stable boxes above. It is not called by them.
```

### Compose availability

Once the Docker socket is removed, `btc-simnet-control-plane` becomes part of the
default stack rather than an opt-in `panel` profile. It is the main operator API and
should be available after an ordinary `docker compose up`.

Mining and spammer must not depend on control-plane availability. The control plane may
depend on node1 health for initial readiness, but it must start and expose diagnostic
status while other workers are unavailable or bootstrapping.

The embedded UI adds negligible deployment complexity. Expensive telemetry such as a
verbose full-mempool fee histogram should use a slower/adaptive sampling interval so
making the control plane default does not create avoidable load under deep spam.

## 6. Public API contract

Keep all JSON routes under `/api/v1`. Use the existing error envelope and extend its
closed error-code enum. Mutations require the bearer token; read-only endpoints remain
tokenless while the server is loopback-only. The complete `/mcp` endpoint requires the
token because its session can reach mutations.

### Health and aggregate state

```text
GET /health/live
GET /health/ready
GET /api/v1/status
GET /api/v1/events?after=<sequence>&limit=<n>
```

- `live` means the server event loop is running.
- `ready` means node1 RPC is reachable and the control-plane state store loaded. Worker
  unavailability is reported in status but does not necessarily make the server dead.
- `status` returns chain/mempool telemetry, component states, active operation, active
  impairments, desired/effective config generations, and mempool.space health/URL.
- `events` provides cursor-based polling over a bounded event ring. SSE/WebSockets are
  deferred; dashboard and CLI can long-poll or poll by sequence.

### Configuration and component state

```text
GET   /api/v1/config/schema
GET   /api/v1/config
PATCH /api/v1/config

PUT   /api/v1/mining/state
PUT   /api/v1/spam/state
```

`PATCH /config` accepts a partial typed/canonical setting map plus an optional
`base_generation`. A stale generation returns `409 stale_revision`. The response says
for each key whether it was applied immediately, will take effect at the next safe
point, required an internal engine rebuild, or is boot-only and rejected.

Manual `PUT state` is desired state (`running` or `paused`), distinct from temporary
job-owned pause leases. Repeating the same desired state is idempotent.

### Jobs

```text
POST /api/v1/jobs/reorg
POST /api/v1/jobs/scenario
POST /api/v1/jobs/partition
POST /api/v1/jobs/degrade
POST /api/v1/jobs/mine
POST /api/v1/jobs/spam-burst

GET  /api/v1/jobs
GET  /api/v1/jobs/{job_id}
GET  /api/v1/jobs/{job_id}/events?after=<sequence>
POST /api/v1/jobs/{job_id}/abort
```

Creation returns `202 Accepted` with a job ID. An optional `Idempotency-Key` header
makes retries return the original job. At most one chain-mutating job runs at a time in
v1; a conflicting request returns `409 operation_in_progress` with the active job ID.
Read-only status calls remain concurrent.

Job states:

```text
starting
running
waiting_at_checkpoint
abort_requested
succeeded
failed
aborted
interrupted
```

Each job records:

- ID, kind, normalized request, creation/start/end timestamps;
- current phase and optional scenario step/checkpoint;
- monotonically sequenced structured events;
- leases and impairments owned by the job;
- result payload or structured failure;
- cleanup outcome separately from the primary outcome.

Disconnecting an HTTP/CLI client does not cancel a server-side job. `abort` is
cooperative: an executor stops at its next safe point and performs owned cleanup. A
reorg that has already invalidated history may finish the minimum safe rewrite before
honoring abort; the API must report `abort_requested`, never claim immediate cancel.

### Scenario checkpoints

```text
GET  /api/v1/jobs/{job_id}/checkpoints/{name}
POST /api/v1/jobs/{job_id}/checkpoints/{name}/release
```

Release is idempotent for the active occurrence. Supplying a stale job/checkpoint
generation returns `409`, preventing a retry from accidentally releasing a later
checkpoint with the same name.

## 7. `simchainctl` CLI

Add a workspace binary:

```text
crates/simchainctl/
  Cargo.toml
  src/main.rs
  src/client.rs
  src/output.rs
  src/commands/
```

The CLI is a thin API client. It does not connect to Docker or Bitcoin RPC directly and
does not duplicate domain validation.

### Connection and authentication

Resolution order:

1. `--url` / `--token` command-line arguments;
2. `SIMCHAIN_CONTROL_URL` / `SIMCHAIN_CONTROL_TOKEN` environment variables;
3. defaults: `http://127.0.0.1:8090` and the local control state token file.

The control-plane state directory is bind-mounted at a narrow path such as
`.simchain-control/`, not the repository root. It contains the bearer token and desired
state, is gitignored, and preserves host-readable ownership/mode. No privileged mount is
needed.

### Required commands

```text
simchainctl status [--watch] [--json]
simchainctl config show [--json]
simchainctl config set KEY=VALUE...

simchainctl mining pause|resume
simchainctl spam pause|resume

simchainctl mine --node node2 --blocks N [--wait]
simchainctl spam burst --node node2 --txs N [--outputs-per-tx N] [--wait]
simchainctl reorg --depth N [--empty] [--wait]
simchainctl partition --node node3 --main-blocks N --isolated-blocks N [--wait]
simchainctl degrade --node node3 --delay-ms N --loss-pct P --seconds N [--wait]

simchainctl scenario start FILE [--json|--id-only]
simchainctl scenario run FILE [--result FILE]          # start + wait terminal
simchainctl scenario wait JOB --checkpoint NAME
simchainctl scenario release JOB CHECKPOINT
simchainctl jobs list [--json]
simchainctl jobs watch JOB [--json]
simchainctl jobs abort JOB
```

Human output is concise and streams new events. `--json` emits stable machine-readable
objects. `--id-only` prints only the job ID for shell capture.

Exit codes:

- `0`: request/job succeeded;
- `1`: server-reported job or operation failure;
- `2`: CLI usage or local file error;
- `3`: API unavailable/authentication failure;
- `4`: wait timeout;
- `5`: job aborted/interrupted or cleanup failed.

The exact numeric values must be unit-tested and documented; CI must never infer
success by parsing human prose.

### Compatibility scripts

Keep familiar scripts, but make them thin clients:

```text
scripts/simulate-reorg.sh -> simchainctl reorg
scripts/spam-burst.sh     -> simchainctl spam burst
scripts/partition.sh      -> simchainctl partition / network status commands
scripts/degrade.sh        -> simchainctl degrade
```

Low-level emergency/operator helpers may remain under an explicitly named `scripts/dev/`
or `scripts/legacy/` path, but normal documentation and scenario execution must not call
Docker from inside a container.

## 8. Dashboard integration

Continue serving the dashboard static files from the control-plane binary. The browser
already uses the HTTP API, so preserve that boundary.

### Retain and extend

- Chain tiles: height, best hash, cadence, mempool count/size/min fee.
- Recent block table and fee histogram.
- Schema-driven settings form and inline validation.
- Last-good snapshot behavior with stale/error indicators.
- Bearer token as the CSRF guard and loopback Host validation.

### Replace current lifecycle language

Remove concepts tied to Compose execution:

- `staged = .env + defaults` becomes `desired` configuration;
- `running container env` becomes component-reported `effective` configuration;
- `pending_restart` becomes `pending_apply` or `effective_generation` drift;
- `Apply recreates ...` becomes an impact preview such as
  `applies immediately`, `next safe point`, or `rebuilds spam engine in-process`;
- Docker container restart counts are replaced by component uptime, reachability,
  phase, policy generation, and last error.

### Add control and job views

Add dashboard sections/tabs for:

1. **Components** — mining/spam state, pause/resume, effective policies, next scheduled
   block, current spam cycle and reconciliation state.
2. **Actions** — manual mine, spam burst, and reorg forms.
3. **Scenarios** — upload YAML, validate, start, show step timeline, checkpoint release,
   abort, and result download.
4. **Network** — partition/degrade controls and current per-node impairment leases.
5. **Jobs** — active job progress/events and bounded recent history.

Mutating controls are disabled while an incompatible mutation job owns the coordinator.
The UI shows why and links to the active job rather than surfacing a generic error.

### mempool.space integration

The dashboard should show whether the local explorer is reachable and provide a
prominent `Open mempool.space` link using `MEMPOOL_WEB_PORT`/configured URL. Where
stable explorer routes exist, recent block hashes and transaction IDs may deep-link.

Do not require an iframe: browser headers and upstream UI assumptions may reject it,
and duplicating mempool.space inside the dashboard creates avoidable coupling. The
intended human workflow is two local tabs: control/telemetry in Simchain, detailed
chain/mempool exploration in mempool.space.

## 9. MCP interface

Keep the existing streamable HTTP `/mcp` service and expand it as a thin adapter over
the same service methods.

Required tools:

```text
get_status
get_config
get_config_schema
set_config
set_mining_state
set_spam_state
start_reorg
start_scenario
start_partition
start_degrade
get_job
list_jobs
release_checkpoint
abort_job
```

Tool annotations must accurately mark read-only, destructive, and idempotent behavior.
MCP does not receive hidden power: it has the same domain scope, mutation lock, auth,
validation, and error envelopes as HTTP and the dashboard.

## 10. Configuration ownership and persistence

Separate configuration into three classes.

### Boot-only infrastructure configuration

Still comes from Compose/environment:

- RPC URLs/credentials and wallet names;
- node images, ports, datadirs, relay/capacity policies;
- control listen addresses and internal endpoints;
- explorer images/ports.

Changing these remains an explicit host/Compose operation.

### Durable runtime desired configuration

Human/API changes to supported mining and spam policies are stored atomically in the
control-plane state directory, not by rewriting the repository `.env`. On startup, the
control plane loads desired state and reconciles it to reachable workers.

The state file contains a schema version, monotonically increasing generation, desired
mining/spam values, and last apply outcome. Use atomic temp-file + rename; no database is
needed. Corrupt state fails visibly and does not silently overwrite worker behavior.

Workers report their effective generation and canonical values. The API/dashboard show
desired and effective independently so a restarting/unreachable worker never appears
successfully retuned.

### Job-scoped overrides

Scenarios may temporarily change mining/spam policies. These changes are owned by the
job and restore the prior desired/effective state during cleanup. They do not mutate the
durable manual configuration unless a future step explicitly requests persistence.

## 11. Internal control protocol

Use versioned HTTP/JSON on `btc-simnet-control`, with no worker control port published to
the host. A small synchronous server thread plus channels is sufficient inside the
currently blocking mining/spam binaries; do not force their engine logic onto Tokio.

Use one shared internal bearer token supplied to the control plane and workers. TLS and
mTLS are unnecessary on the local Compose network in v1. Requests include a unique
request ID; repeated IDs are idempotent.

Shared protocol DTOs and state enums belong in `simchain-common` once used by more than
one crate.

### Common endpoints

```text
GET  /internal/v1/status
POST /internal/v1/leases
POST /internal/v1/leases/{id}/renew
DELETE /internal/v1/leases/{id}
```

A lease includes owner job ID, purpose, TTL, request ID, and whether release reports
`chain_changed`. A pause acknowledgement means the worker is at a documented safe point,
not merely that a flag was set.

Manual desired pause is not a lease. Effective state is paused when manual desired state
is paused **or** at least one valid lease exists. Releasing one job lease must never
resume a manually paused worker or a worker held by another lease.

### Lease crash recovery

- The control plane renews job leases periodically.
- Expiry causes conservative reconciliation when chain history may have changed.
- The worker resumes only after successful reconciliation.
- Failed reconciliation keeps the worker paused/erroring and visible; it must not resume
  with known-stale state.

## 12. Mining-controller changes

Split current `MiningConfig` into:

- static connection/bootstrap configuration;
- typed `MiningPolicy` based on shared `MiningTuning`;
- mutable control state: desired state, active leases, policy generation, phase, and
  recent telemetry.

State machine:

```text
bootstrapping -> running -> pausing -> paused -> running
                         \-> error
```

Required behavior:

- Bootstrap remains stage-resumable and is not paused halfway through an unsafe funding
  mutation. Status reports `bootstrapping`; pause may wait until the current safe stage.
- Replace the uninterruptible mining sleep with a condition variable/channel wait that
  wakes on pause, shutdown, or policy generation change.
- A pause acknowledgement waits for any in-flight `generate` RPC and propagation check
  to finish.
- Applying cadence/weights wakes the scheduler and resamples from the new policy rather
  than waiting out an interval calculated from stale configuration.
- Report phase, height, effective policy/generation, next scheduled attempt, last mined
  block, active leases, uptime, and last error.
- A control-plane outage leaves the last effective policy running.

`MINING_RNG_SEED` needs explicit semantics on live change. Pin v1 behavior: changing it
reinitializes the RNG at the next safe scheduler boundary and resets the alternation
toggle deterministically; status/event output records the new generation and seed.

## 13. Spammer changes

Split current configuration into static RPC/wallet identity and mutable `SpamPolicy`
based on shared `SpamTuning`.

State machine:

```text
initializing -> active -> pausing -> paused -> reconciling -> active
                    \-> disabled                    \-> error
```

Unlike today, `ENABLE_SPAM=false` must leave the long-running process alive in
`disabled`, ready to be enabled through its API. It must not exit successfully, because
that would make API re-enable impossible without Docker lifecycle control.

Required safe points:

- before a block-triggered cycle;
- between floor-pool, small-tx, bulk DATA/OUTPUT, and RBF phases;
- inside long per-transaction loops at points where in-memory branch state is
  consistent;
- after an already-submitted RPC batch is accounted for.

Classify policy changes:

1. **Immediate/next cycle:** enabled state, fill ratio/target, floor/small/RBF counts
   where existing engine structures remain valid.
2. **Safe-point engine rebuild:** raw versus wallet engine, fee/data/output shape, or
   fanout changes requiring new engine objects or reconciliation.
3. **Boot-only:** RPC URLs, wallet names, identities.

The worker validates and acknowledges a complete new policy before replacing the old
one. On rebuild failure, keep or restore the previous valid engine/policy and return a
structured rejection. Never leave a half-reconfigured engine active.

Report phase, observed height, effective policy/generation, current cycle phase,
accepted transaction count, branch/floor reconciliation status, active leases, uptime,
and last error.

## 14. Reorg execution

Do not add a resident reorg API service. Convert `simchain-reorg` into a library plus a
thin compatibility binary:

```text
crates/reorg/src/lib.rs
  ReorgRequest
  ReorgResult
  ReorgObserver / structured progress callback
  run_once(...)

crates/reorg/src/main.rs
  env/CLI adapter -> library
```

Remove core dependence on global `OnceLock` configuration. The control plane invokes
the library from a blocking job worker and converts observer callbacks into job events.

Example public request:

```json
{
  "depth": 3,
  "empty": true,
  "node": "node3",
  "adds_new_txs": 0,
  "double_spend_pct": 0
}
```

The job acquires the mutation coordinator and any required mining/spam leases before
history mutation, holds them through witness convergence, and releases with
`chain_changed=true`. Existing reorg race-tolerance remains defense in depth; the
control plane uses deterministic coordination by default.

Auto-reorg scheduling should eventually live in the control plane as recurring jobs.
The existing `REORG_MODE=auto` binary may remain compatible during migration, but it is
not part of the final primary operator path.

## 15. Scenario engine and CI checkpoints

Refactor the scenario package into a reusable library plus thin compatibility/client
binary. The server-side executor receives a domain action interface, not `Docker`:

```text
trait ScenarioActions {
    wait_height(...)
    set_mining_state(...)
    mine(...)
    run_reorg(...)
    spam_burst(...)
    run_partition(...)
    reach_checkpoint(...)
}
```

The control-plane implementation calls worker APIs, Bitcoin RPC, and job executors.
Tests use a fake action backend. Delete the final dependency on
`scenario-engine/src/docker.rs` after migration.

### Submission

`POST /api/v1/jobs/scenario` accepts YAML text or a JSON envelope containing YAML. The
server parses and validates it before reserving the mutation coordinator. No scenario
file or repository bind mount is required.

Existing version-1 files remain valid and behavior-compatible.

### Additive checkpoint step

Add this step without changing existing scenario semantics:

```yaml
version: 1
steps:
  - type: pause_mining

  - type: spam_burst
    node: btc-simnet-node2
    txs: 500
    outputs_per_tx: 25

  - type: checkpoint
    name: mempool_loaded
    pause: true
    timeout_secs: 600

  - type: resume_mining
```

Rules:

- `name` is non-empty, URL-safe, and unique within one scenario.
- `pause` defaults to `true`. `false` emits a durable milestone event and continues.
- `timeout_secs` is positive and required for a pausing checkpoint in v1; this prevents
  an abandoned CI run from holding the simulation forever.
- On arrival, record a checkpoint generation and full live summary before notifying
  waiters.
- A pausing checkpoint transitions the job to `waiting_at_checkpoint` and waits for
  release, abort, or timeout.
- Timeout fails the scenario and runs owned cleanup.
- Release is idempotent for the same checkpoint generation.

### CI workflow

```bash
job="$(simchainctl scenario start scenarios/ci.yml --id-only)"
trap 'simchainctl jobs abort "$job" >/dev/null 2>&1 || true' EXIT

simchainctl scenario wait "$job" --checkpoint mempool_loaded --timeout 600

# Test the downstream system while Simchain is held at exactly this state.
cargo test -p downstream-integration

simchainctl scenario release "$job" mempool_loaded
simchainctl jobs watch "$job" --timeout 900

trap - EXIT
```

`scenario run` remains the simple start-and-wait command for scenarios without external
barriers. `--result path.json` writes the server result and event/checkpoint summary as
a CI artifact.

### Cleanup ownership

Replace boolean stop/start tracking with owned leases and impairment IDs. A scenario
releases only resources it acquired. Cleanup order is:

1. heal active network impairment;
2. wait for required chain convergence when possible;
3. release spam lease with correct `chain_changed` flag;
4. release mining lease;
5. restore job-scoped policy overrides;
6. record cleanup errors separately from the primary failure.

## 16. Partition and network-agent design

Add one small network-agent binary/image, instantiated once per node with:

- `network_mode: service:<node>`;
- `CAP_NET_ADMIN` only;
- internal control API, no host port;
- no Docker socket and no independent Docker network membership.

The agent owns a lease-based impairment state:

```text
clear
netem { delay_ms, loss_pct }
partition { ingress_drop, egress_drop }
```

Hard partition must block both ingress and egress on the P2P interface while preserving
the control-network path. Do not model a hard partition only as egress `loss 100%`.
Use nftables/iptables/tc primitives available in the helper image, and detect the P2P
interface from its route as the current helper does.

The control plane coordinates a deterministic partition job:

1. validate post-bootstrap and converged starting tips;
2. acquire mining and default spam pause leases;
3. acquire/renew a network impairment lease;
4. ask Bitcoin RPC to disconnect existing P2P peers so stale TCP sessions flush;
5. verify target isolation and main-side connectivity;
6. mine requested branch lengths over control RPC;
7. clear impairment and trigger reconnect;
8. verify the expected winning tip on all nodes;
9. release worker leases and report result.

Agent TTL expiry clears impairment automatically if the control plane dies. This is the
network equivalent of worker pause-lease recovery and prevents abandoned partitions.

Persistent `NET_ADMIN` in a node namespace is a deliberate tradeoff relative to the
current short-lived helper. It is much narrower than Docker-socket access and is needed
to remove Docker as the runtime network-control API. Document it prominently.

## 17. Job coordinator, events, and persistence

Use a single in-process mutation coordinator, not a general queue. V1 either starts a
job immediately or returns `409` with the active job. This is deterministic and avoids
surprising delayed mutations.

Persist small job metadata/results under the control state directory:

- active job marker and normalized request;
- terminal summaries for the most recent 100 jobs;
- append-only JSONL event file per active/recent job, bounded/rotated;
- scenario result/checkpoint summary.

On control-plane restart:

- mark a non-terminal previous job `interrupted`;
- do not attempt to resume an arbitrary partially completed workflow;
- query worker/network statuses and wait for lease TTL recovery or explicitly heal
  resources that still identify the interrupted job;
- expose recovery progress/errors in status;
- refuse a new mutation until recovery reaches a safe terminal state.

Use an in-memory event ring for fast polling and the file artifact for restart/history.
No SQL database is required.

## 18. Failure and concurrency semantics

| Failure | Required behavior |
| --- | --- |
| Control plane unavailable | Workers keep last effective state; Docker may restart the control plane. |
| Worker unavailable before job mutation | Fail before mutation or remain explicitly pending; never assume pause/config success. |
| Pause safe-point timeout | Fail before dependent mutation and release already acquired resources. |
| Control plane dies while paused | Lease expires; worker reconciles if needed, then resumes unless manually paused. |
| Control plane dies during partition | Network-agent TTL clears impairment; workers recover from leases. |
| Reorg fails after invalidation | Reorg executor completes minimum safe cleanup/rewrite, reports primary and cleanup failures. |
| Scenario checkpoint times out | Fail job and run owned cleanup. |
| CI client disconnects | Server job/checkpoint continues until release, timeout, or explicit abort. |
| Runtime config rejected by worker | Keep previous effective policy; desired/effective status must not falsely match. |
| Worker restarts | Docker restarts it from boot config; control plane detects generation drift and reapplies durable desired policy. |
| Manual config change during mutation job | Return `409 operation_in_progress` unless the change is owned by that job. |
| Duplicate request/idempotency key | Return original operation/job without executing twice. |
| Status sampling fails | Keep last good data, mark stale, preserve independent RPC/component/explorer error fields. |

## 19. Dashboard-branch file migration

### Rename/reframe

| Current | Target |
| --- | --- |
| `crates/panel` | `crates/control-plane` |
| package `simchain-panel` | `simchain-control-plane` |
| service `btc-simnet-panel` | `btc-simnet-control-plane` |
| `.panel-token` | `.simchain-control/token` (or equivalent narrow state directory) |
| `PANEL_WEB_PORT` | `CONTROL_PLANE_PORT`, with temporary alias if needed |

### Retain and extend

| File/area | Change |
| --- | --- |
| `api.rs` | Keep auth/static/API structure; add config, component, job, checkpoint, and event routes. |
| `service.rs` | Preserve transport-independent pattern; split into domain services as it grows. |
| `mcp.rs` | Add control/job tools over the same service functions. |
| `status.rs` | Keep node metrics; replace Docker inspection with worker/network status and active-job data. |
| `static/*` | Keep schema-driven UI; update semantics and add component/action/scenario/network/job views. |
| `simchain-common/live_tuning.rs` | Preserve shared catalog/validators; add serde/API metadata and runtime apply classification. |
| token/Host security | Preserve loopback binding, Host validation, token secrecy, and mutation guards. |

### Replace or remove

| Current file/behavior | Final treatment |
| --- | --- |
| `panel/src/apply.rs` | Replace Compose transaction with desired/effective runtime config service and worker clients. |
| `panel/src/compose.rs` | Delete after the last transitional Docker-backed operation migrates. |
| `panel/src/docker_inspect.rs` | Delete; component APIs report domain state. |
| `panel/src/envfile.rs` | Refactor into narrow control-state/token atomic storage; stop rewriting repo `.env`. |
| Docker CLI packages in panel image | Remove. |
| repo root bind mount | Remove; mount only narrow control state if host token discovery is needed. |
| Docker socket mount | Remove with no replacement proxy. |

### Other crate changes

| Area | Change |
| --- | --- |
| `simchain-common` | Add shared internal control DTOs, leases, phases, config generations, job/event primitives used by multiple crates. |
| `mining-controller` | Split static/runtime config, control server/channel, interruptible scheduler, phase/status telemetry. |
| `spammer` | Stay alive when disabled, control server, leases, cooperative pause points, policy rebuild/reconciliation. |
| `reorg` | Library API with explicit request/result/observer; retain thin CLI. |
| `scenario-engine` | Library API, control-action backend, checkpoint step, server job execution; remove Docker backend. |
| new `simchainctl` | First-party HTTP client and stable CI exit semantics. |
| new network agent | Internal API and leased namespace-local netem/partition implementation. |
| `docker-compose.yml` | Default control-plane service, worker internal endpoints, network agents, control state; no socket mounts. |
| `docker/tools.Dockerfile` | Build new binaries; control-plane runtime has no Docker CLI. |
| scripts | Convert normal operator helpers to CLI/API wrappers. |
| docs/AGENTS | Update crate/tool inventory and verification guidance after new crates land. |

Keep per-crate dependency declarations and update `Cargo.lock` with every dependency
change. Do not introduce `[workspace.dependencies]`.

## 20. Recommended implementation sequence

Keep the control-plane branch functional at each phase, but do not declare the migration
complete while any in-container Docker socket path remains.

### Phase 1 — Integration foundation

1. Rename/reframe panel as control plane while retaining current behavior temporarily.
2. Introduce backend traits for component status/config and job actions so API/UI tests
   no longer depend directly on `compose::Executor`.
3. Add control-state storage and shared API DTO modules.
4. Add read-only `simchainctl status/config` and preserve HTTP/MCP parity.
5. Pin the new API contract with handler/service/client tests.

### Phase 2 — Mining runtime control

1. Refactor mining static/runtime policy.
2. Add internal server/channel, safe pause leases, status, and interruptible scheduling.
3. Switch control-plane mining state/config paths from Compose to the internal API.
4. Update UI/CLI/MCP and verify container identity remains unchanged through pause/tune.

### Phase 3 — Spammer runtime control

1. Add resident disabled state and internal control server.
2. Add lease state machine and cooperative pause points.
3. Implement hot changes and safe-point engine rebuild/reconciliation.
4. Switch control-plane spam config/state from Compose to the internal API.
5. Update UI/CLI/MCP and verify container identity remains unchanged.

### Phase 4 — Job framework and reorg

1. Add mutation coordinator, job/event/result store, idempotency, abort token, and API.
2. Extract reorg library request/result/progress interface.
3. Implement reorg job with worker leases and witness convergence.
4. Add CLI, MCP, and dashboard reorg/progress paths.

### Phase 5 — Server-side scenarios and CI barriers

1. Extract scenario library/action interface.
2. Add checkpoint schema/state/release/timeout and tests.
3. Implement scenario upload/job execution/result artifacts.
4. Add full CLI CI workflow and dashboard scenario timeline/control.
5. Migrate wait/mine/burst/reorg steps off Docker.

### Phase 6 — Network agents

1. Implement namespace-local agent API, leased netem, hard partition, status, and TTL
   healing.
2. Implement control-plane partition/degrade jobs.
3. Migrate scenario partition step and scripts.
4. Verify no operation needs Docker inspection/network connect/disconnect at runtime.

### Phase 7 — Remove transitional Docker control

1. Delete Compose/Docker-inspect executors and scenario Docker backend.
2. Remove Docker CLI packages, repository bind mounts, and every Docker socket mount.
3. Make control plane part of default Compose startup.
4. Finish dashboard component/job/network views and mempool.space integration.
5. Update all documentation and examples to UI/CLI/API/MCP paths.

## 21. Automated verification

### Shared/unit tests

- Existing live-tuning parsing/canonicalization/validation remains identical across
  control plane and workers.
- Runtime apply classification for every managed setting.
- Mining state machine, interruptible sleep, policy generation, pause safe points, and
  lease expiry.
- Spammer lease idempotency/conflict/renewal/expiry, cooperative safe points,
  reconciliation, disabled re-enable, and rebuild rollback.
- Job state transitions, one-mutation lock, idempotency keys, event cursors, persistence,
  restart interruption, and cleanup reporting.
- Scenario checkpoint validation, arrival, wait, release idempotency, timeout, abort,
  and external-client disconnect behavior.
- Network-agent validation, interface targeting, lease TTL, and command construction
  behind a mock system adapter.
- HTTP auth/error/status contracts, MCP parity, and CLI command/exit-code mapping.

### Docker integration tests

1. Start ordinary Compose and confirm control plane is ready without a profile.
2. Assert no service mounts `/var/run/docker.sock` and the control-plane image contains
   no Docker CLI.
3. Record mining/spammer container IDs and restart counts.
4. Pause/resume both through API; IDs/counts remain unchanged.
5. Change cadence and fill ratio; effective generations update and IDs/counts remain
   unchanged.
6. Disable and re-enable spam without recreating/restarting its container.
7. Run `reorg --depth 3 --empty`; verify a successful job and network convergence.
8. Run each shipped scenario through the control plane.
9. Run a checkpoint scenario, execute an external RPC assertion while held, release it,
   and verify terminal success/result artifact.
10. Kill the CI client while waiting; scenario stays held until timeout/another client
    releases it.
11. Kill the control plane while worker leases are held; TTL reconciliation restores a
    safe state and restart marks the job interrupted.
12. Kill the control plane during a partition; network-agent TTL heals the P2P path.
13. Run dashboard/API/MCP/CLI operations and compare normalized results.
14. Run with mempool profile and verify explorer health/link plus visible effects of
    cadence/fill changes.

### Repository gates

From the workspace root:

```bash
cargo ba && cargo ca && cargo fac && cargo tt
```

Also validate Compose configuration, build every Docker target with the committed lock
file, and run the no-socket inspection test.

## 22. Manual acceptance flows

### Human dashboard flow

1. `docker compose --profile mempool up -d --build`.
2. Open the Simchain dashboard and local mempool.space.
3. Confirm chain/component/job status is live.
4. Change block mean and spam fill ratio; no container recreation occurs.
5. Watch cadence/block fullness change in the dashboard and mempool.space.
6. Pause mining, create a burst, inspect the mempool, then resume.
7. Request a three-block empty reorg and follow its progress/convergence.
8. Upload a scenario, observe its step timeline, release a checkpoint, and inspect the
   result/history.

### CI flow

1. Start Compose and wait with `simchainctl status`/readiness.
2. Submit scenario and capture ID.
3. Wait for named checkpoint with a timeout.
4. Run downstream tests.
5. Release and wait for completion.
6. Save JSON result/events.
7. Confirm a failing scenario or downstream-abort cleanup produces the documented
   non-zero result and does not leave mining, spam, or P2P unintentionally paused.

## 23. Acceptance criteria

The control-plane feature is complete only when all are true:

- `docker compose up` starts a reachable control plane with UI, API, and MCP.
- No Simchain service mounts the Docker socket; no control/scenario image includes or
  invokes Docker CLI/Compose.
- Docker remains the process supervisor, while pause/resume/config/action semantics use
  application APIs and jobs.
- Mining cadence/weights and supported spam policies change without container restart.
- Mining and spam can be paused safely without container stop, including job-owned
  leases that do not override manual pause state.
- `POST /api/v1/jobs/reorg` can execute a three-block empty reorg and report structured
  progress/result.
- Scenarios run server-side from uploaded YAML, retain existing examples, and no longer
  need a repo bind mount.
- A CI client can wait at a named checkpoint, test an external system, release the
  scenario, and receive a stable result/exit code.
- The dashboard exposes chain/mempool telemetry, desired/effective settings, component
  phases, active job/step/checkpoint progress, network impairment state, and recent
  results.
- The dashboard links to and reports health for local mempool.space.
- `simchainctl`, HTTP, MCP, and dashboard adapters execute the same service-layer logic.
- Worker/control-plane/network-agent crash cases recover through leases/TTL or surface a
  truthful error without silently resuming stale state.
- Current snapshot/node persistence behavior and mainnet-like policy constraints remain
  unchanged.
- CI-equivalent Cargo checks, Compose validation, Docker builds, integration tests, and
  no-socket assertions all pass.

## 24. Key risks and mitigations

- **Spammer safe-point latency:** raw cycles can run for tens of seconds. Add checks
  inside existing transaction loops and expose `pausing` progress rather than lying or
  force-killing state.
- **Control-plane single point of control:** keep it out of the data path; workers retain
  effective behavior and leases have TTL recovery.
- **Desired/effective drift after worker restart:** version worker policies and run a
  reconciliation loop; display both values.
- **Interrupted consensus-sensitive jobs:** persist active metadata, use typed phases,
  perform minimum safe cleanup, and block new mutations until recovery completes.
- **Persistent namespace privilege:** network agents hold only `NET_ADMIN` in node
  namespaces, expose a narrow authenticated API, and never access Docker/host namespaces.
- **Default telemetry cost:** cache status and sample verbose mempool data adaptively.
- **API/UI drift:** keep browser, CLI, and MCP as adapters over shared service methods and
  contract-test normalized responses.
- **Scope growth:** keep one mutation coordinator, bounded jobs, additive scenario
  checkpoints, and explicit non-goals; do not evolve this into a general workflow or
  infrastructure platform.

## 25. Final design rule

When adding a future capability, decide its owner before adding an endpoint:

- process crash/restart/network creation -> Docker Compose;
- chain/wallet/mempool truth -> Bitcoin Core RPC;
- mutable algorithm state and safe points -> the owning worker;
- node-network impairment -> network agent;
- sequencing, desired behavior, leases, user intent, and observability -> control plane;
- presentation/automation -> dashboard, CLI, MCP, or HTTP client.

If an implementation would require giving the control plane or scenario runner the
Docker socket again, treat that as an architecture regression requiring an explicit new
design decision, not as a convenient shortcut.
