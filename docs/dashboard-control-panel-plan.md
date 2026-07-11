# Implementation plan: dashboard / control panel

## Status: READY TO IMPLEMENT (written 2026-07-10)

Implements nice-to-have **"4. Dashboard / control panel"** from
[NICE-TO-HAVE.md](NICE-TO-HAVE.md). That entry explains the user-facing goal; this
document is the engineering hand-off: exact scope, architecture, file-level changes,
and the verification plan.

## 1. Goal and non-goals

**Goal:** ship one optional, localhost-only web UI that:

- shows live simnet state from `node1` RPC: height, recent blocks, observed block
  cadence, mempool depth, fee distribution, and service state.
- shows the current retuning knobs in one place.
- lets the user change the live-retunable settings and apply them with one click.
- applies changes by rewriting `.env` and recreating only the affected tool services,
  matching the manual flow in [RETUNING.md](RETUNING.md).
- rolls back automatically if the apply step fails after `.env` has been written.

The first supported workflow is:

```bash
docker compose --profile panel up -d
```

Then visit `http://localhost:8090` in a browser and:

1. inspect the current chain state,
2. change mining cadence / miner weights / spam settings,
3. click Apply,
4. watch the effect on the live mempool and block stream.

**Non-goals:**

- No general-purpose Docker dashboard. The panel manages only simchain's live-retune
  settings and the two tool services they affect.
- No editing of node-level settings that require recreating the nodes
  (`BTC_IMAGE`, host ports, `MIN_RELAY_TX_FEE`, ZMQ ports, `BLOCK_RESERVED_WEIGHT`,
  `NODE1_DISABLE_WALLET`, ...). Those remain manual because they are not safe "live
  retunes" and can reset or materially alter the chain environment.
- No remote exposure or multi-user auth. v1 is explicitly localhost-only and behind
  an opt-in compose profile.
- No JS build pipeline, SPA framework, or Node-based frontend toolchain. The repo is
  Rust-only today; the panel should stay that way.
- No attempt to make `.env` a universal source of truth for every runtime detail.
  The panel owns only the hot-retunable subset.

## 2. Current state (what the plan builds on)

- **Live retuning already exists, but only manually.**
  [RETUNING.md](RETUNING.md) documents the exact workflow today: edit `.env`, then run
  `docker compose up -d --force-recreate` on
  `btc-simnet-mining-controller` and/or `btc-simnet-spammer`.
- **Only the tool containers are safe live targets.** The controller is resumable past
  bootstrap and the spammer is stateless between cycles; recreating either preserves
  the chain because the node datadirs stay on named volumes. Recreating nodes is a
  different class of operation and is out of scope here.
- **`FALLBACK_FEE` is intentionally awkward.** It is consumed by the spammer and also
  passed to the nodes as `-fallbackfee` in `docker-compose.yml`. The current manual
  workflow treats it as a live spam-price knob: recreating only the spammer moves the
  fee floor immediately, while the nodes keep their old wallet fallback until a full
  node restart. The panel must preserve that behavior rather than silently resetting
  nodes.
- **The repo has no frontend stack.** The workspace contains only Rust crates; there
  are no existing JS, CSS, or templating assets. `docker/tools.Dockerfile` currently
  builds three binaries from one shared builder stage.
- **Validation logic already exists, but it is process-env-only.**
  `crates/mining-controller/src/config.rs` and `crates/spammer/src/config.rs` already
  contain the rules we want, but they read from process env directly, so the panel
  cannot safely reuse them without refactoring.
- **The compose file is static and explicit.** Service names, container names, and the
  variable-to-service wiring are all hard-coded in `docker-compose.yml`, which makes a
  static mapping practical.
- **The panel has all the data it needs locally.** `node1` RPC provides chain and
  mempool status; `docker inspect` can provide service state and running env for the
  controller and spammer; `.env` is local and writable.

## 3. User-visible v1 behavior

The panel should expose three areas:

### A. Chain status

- Current height and best block hash.
- Last 10 blocks with timestamp delta, tx count, size/weight.
- Rolling observed cadence over the last 10 deltas.
- Mempool summary: tx count, total bytes / usage, minimum mempool fee.
- Fee histogram bucketed from the live mempool.

### B. Current settings

- Mining settings: interval mode, mean, min, max, miner weights, optional RNG seed.
- Spam settings: enabled/disabled, raw vs wallet engine, fee floor, fill ratio, data
  size bounds, floor pool, small tx count, fanout mode, RBF toggle, and the other
  knobs already documented in [SETTINGS.md](SETTINGS.md).
- For each setting, show:
  - the current staged value from `.env` + defaults,
  - the running value on the live container,
  - whether the field is currently "dirty" (edited in browser but not applied).

### C. Apply controls

- One Apply button for the whole form.
- Clear feedback on which service(s) will be recreated:
  - mining only,
  - spammer only,
  - both,
  - or no-op.
- Inline validation errors before any file write.
- A visible warning on `FALLBACK_FEE`: live apply updates the spammer immediately, but
  node wallet fallback does not change until a full node restart outside the panel.

## 4. Architecture overview

Implement the panel as a fourth workspace binary:

- crate: `crates/panel`
- package name: `simchain-panel`
- compose service name: `btc-simnet-panel`
- compose profile: `panel`
- host URL: `http://localhost:${PANEL_WEB_PORT:-8090}`

Runtime model:

1. `axum` serves a single HTML page plus a small static JS/CSS bundle embedded in the
   Rust binary.
2. A background status sampler polls `node1` RPC and Docker on a fixed interval and
   stores the last good snapshot in memory.
3. The browser polls lightweight JSON endpoints.
4. An apply request validates the proposed settings, rewrites `.env` atomically,
   recreates only the necessary service(s), verifies the result, and rolls back on
   failure.

This stays aligned with the rest of the repo:

- Rust only.
- One binary in one container.
- No changes to mining, spam, or reorg behavior when the `panel` profile is not used.

## 5. Change 1: extract shared live-retune validation into `simchain-common`

Do not duplicate the mining/spam validation rules in the panel. That would drift almost
immediately.

Add a new shared module in `crates/simchain-common`, for example:

```text
crates/simchain-common/src/live_tuning.rs
```

It should provide:

- a `SettingSpec` catalog for the panel-managed variables:
  - env var name,
  - default string value,
  - section/group,
  - restart scope (`mining-controller`, `spammer`, or special-case shared),
  - UI control type,
  - short help text.
- source-agnostic parsing helpers that work from an in-memory key/value map, not just
  `std::env`.
- typed validation for the live-retunable mining subset.
- typed validation for the live-retunable spam subset.
- canonical serialization back to env-string form.

### Managed variables in v1

Mining controller scope:

- `BLOCK_INTERVAL_MODE`
- `BLOCK_INTERVAL_MEAN_SECS`
- `BLOCK_INTERVAL_MIN_SECS`
- `BLOCK_INTERVAL_MAX_SECS`
- `MINER_WEIGHTS`
- `MINING_RNG_SEED`

Spammer scope:

- `ENABLE_SPAM`
- `USE_RAW_TX_SPAM`
- `FALLBACK_FEE`
- `SPAM_FIXED_TXS_PER_BLOCK`
- `SPAM_SENDMANY_OUTPUTS`
- `SPAM_TX_DATA_MAX_BYTES`
- `SPAM_TX_DATA_MIN_BYTES`
- `SPAM_SMALL_TXS_PER_BLOCK`
- `SPAM_FLOOR_POOL_TXS`
- `SPAM_FILL_BLOCK_RATIO`
- `SPAM_FANOUT_AUTO`
- `SPAM_FANOUT_UTXOS`
- `ENABLE_SPAM_REPLACES`
- `SPAM_REPLACES_PER_MINER_PER_BLOCK`

Explicitly **exclude** from the panel:

- `USER_ADDRESS`
- `NODE{1,2,3}_RPC_URL`
- `NODE{2,3}_WALLET_NAME`
- RPC credentials
- node image / node policy / host port / ZMQ / explorer settings

Those are either not live-retunable or are too easy to misuse from a browser control
surface.

### Refactor target

`crates/mining-controller/src/config.rs` and `crates/spammer/src/config.rs` should be
updated to reuse the shared validators/default tables for the overlapping fields, so the
panel and the binaries continue to enforce exactly the same rules.

The point of this refactor is not code tidiness; it is to prevent the panel from
accepting a configuration that the actual tool container will then reject on restart.

## 6. Change 2: add a new `crates/panel` binary

Create a new crate with these modules:

```text
crates/panel/
  Cargo.toml
  src/
    main.rs
    app.rs
    api.rs
    compose.rs
    docker_inspect.rs
    envfile.rs
    state.rs
    status.rs
    apply.rs
  static/
    index.html
    app.js
    styles.css
```

Suggested responsibilities:

- `main.rs`
  - load `.env` with `dotenvy`,
  - initialize tracing,
  - load panel config (listen addr, repo root, env file path),
  - start the status sampler task,
  - start the `axum` server.
- `state.rs`
  - shared app state (`Arc`), cached status snapshot, current apply lock.
- `envfile.rs`
  - `.env` parsing, canonicalization, revision hashing, atomic writes.
- `compose.rs`
  - spawning `docker compose ... up -d --force-recreate ...`.
- `docker_inspect.rs`
  - inspecting running containers for state and env.
- `status.rs`
  - RPC sampling and in-memory status snapshots.
- `apply.rs`
  - validate -> write -> recreate -> verify -> rollback flow.
- `api.rs`
  - HTTP handlers and JSON response types.

### Dependencies

Each crate declares its own dependencies, so keep this local to `crates/panel/Cargo.toml`.
Expected dependencies:

- `axum`
- `tokio`
- `serde`
- `serde_json`
- `dotenvy`
- `simchain-common`
- `anyhow`
- `tracing`
- `tempfile` (tests)
- optional small helpers like `sha2` for revisions

`bitcoincore-rpc` stays blocking, just like the existing tools. All RPC calls and
process execution in request handlers or sampler tasks must therefore run in
`tokio::task::spawn_blocking` so the async server does not stall.

## 7. Change 3: `.env` reading/writing with conflict detection

The panel must be able to read the live-retune subset even when `.env` does not exist,
because the compose file supplies defaults.

### Read path

Build the staged settings view by:

1. parsing `.env` if present,
2. overlaying its values on top of the shared default table from §5,
3. validating the resulting managed subset through the shared parser,
4. surfacing a clear page-level error if the file contains an invalid value.

### Write path

Do **not** try to preserve arbitrary original formatting of managed keys in place. That
is fragile and buys little. Use a canonical managed block instead.

On apply:

1. read the current file contents and compute a `base_revision` hash,
2. reject the request with `409 Conflict` if the client's revision is stale,
3. remove all existing occurrences of the panel-managed keys from the file,
4. preserve every unmanaged line verbatim,
5. append one canonical block at the end:

```dotenv
# Managed by simchain panel
BLOCK_INTERVAL_MODE=poisson
BLOCK_INTERVAL_MEAN_SECS=15
...
```

6. write atomically via temp file + rename.

This gives deterministic output, eliminates duplicate-key ambiguity, and avoids a large
"preserve every comment style ever written by hand" parser project.

### File ownership / locking

- Serialize applies with an in-process mutex.
- Also create a simple lock file next to `.env` during the write/apply window so a
  second panel process cannot race it.
- If `.env` does not exist, create it on first successful apply with only the managed
  block plus a short header comment.

## 8. Change 4: Docker and compose integration

### Workspace

Update the root `Cargo.toml` workspace members to include `crates/panel`.

### Docker build

Extend `docker/tools.Dockerfile` with a fourth final target for the panel.

Unlike the existing distroless / slim final stages, the panel image needs:

- the Rust panel binary,
- the Docker CLI,
- the compose plugin,
- CA certs.

Use a Debian-based final stage so the glibc-linked Rust binary runs cleanly. Do **not**
copy the binary into `docker:cli` or another Alpine-based image unless the build is
changed to a compatible target; with today's builder that is the wrong base.

### Compose service

Add a new service to `docker-compose.yml`:

- service name: `btc-simnet-panel`
- profile: `panel`
- container name: `btc-simnet-panel`
- build target: `panel`
- port binding: `127.0.0.1:${PANEL_WEB_PORT:-8090}:8080`
- bind mount repo root: `.:/workspace`
- mount Docker socket: `/var/run/docker.sock:/var/run/docker.sock`
- environment:
  - `SIMCHAIN_REPO_ROOT=/workspace`
  - `SIMCHAIN_ENV_FILE=/workspace/.env`
  - `PANEL_LISTEN_ADDR=0.0.0.0:8080`
- optional `depends_on`:
  - `btc-simnet-node1` healthy

Do **not** add the panel to `all-tools`. It exposes `docker.sock`, so it must stay an
explicit opt-in profile.

### Compose command the panel runs

Use the same compose file the user runs manually:

```bash
docker compose \
  -f /workspace/docker-compose.yml \
  --project-directory /workspace \
  up -d --force-recreate <services...>
```

Notes:

- No `--remove-orphans`.
- No attempt to reconstruct all active profiles; the only recreate targets in v1 are
  the unprofiled base services, so extra profile services stay untouched.
- Use the compose *service names*, not container names.

## 9. Change 5: running-state inspection and accurate service mapping

Do not infer "current settings" only from `.env`. That is wrong whenever a user has
edited `.env` manually but has not recreated the tool containers yet.

Use `docker inspect` on the pinned container names:

- `btc-simnet-mining-controller`
- `btc-simnet-spammer`
- `btc-simnet-node1`
- optionally `btc-simnet-node2`
- optionally `btc-simnet-node3`

From inspect data, extract:

- `State.Status`
- `State.Running`
- `State.Restarting`
- `RestartCount`
- the effective container `Env`

The panel should therefore render both:

- **staged values**: `.env` + defaults
- **running values**: what the controller/spammer containers were actually started with

and compute service impact from the running values:

- if only mining fields differ, recreate `btc-simnet-mining-controller`,
- if only spam fields differ, recreate `btc-simnet-spammer`,
- if both differ, recreate both,
- if nothing differs, no-op.

### Special case: `FALLBACK_FEE`

For live retuning, map `FALLBACK_FEE` to **spammer only**, even though the nodes also
consume it in compose.

Reason:

- that matches the documented manual behavior today,
- recreating nodes is outside the scope of "retune a live chain",
- resetting nodes from a browser button would be the wrong default.

The UI must show this as an explicit warning, not as an invisible implementation detail.

## 10. Change 6: status sampling model

Keep the HTTP handlers cheap. Do not hit RPC from every browser poll.

Run a background sampler that refreshes a shared `StatusSnapshot`:

- every 2 seconds:
  - `getblockcount`
  - `getbestblockhash`
  - `getmempoolinfo`
  - controller/spammer container state
- every 5 seconds:
  - recent block list
  - observed cadence
  - mempool fee histogram

If a sample fails:

- keep the last good snapshot,
- mark it stale with `last_updated`,
- surface the error string in the status JSON so the UI can show "stale / RPC unavailable"
  rather than going blank.

### RPC details

Status should come from `node1` RPC only:

- height: `getblockcount`
- best hash: `getbestblockhash`
- recent blocks: `getblockhash` + `getblock` for the last 10 heights
- cadence: timestamp deltas across those recent blocks
- mempool summary: `getmempoolinfo`
- fee histogram: `getrawmempool true`, bucketed by sat/vB

The histogram does not need to be fancy. Fixed buckets are enough, for example:

- `< 5`
- `5-10`
- `10-20`
- `20-50`
- `50-100`
- `100+`

The panel is for operational feedback, not explorer-grade analytics.

## 11. Change 7: HTTP API and browser UI

Prefer plain polling over SSE or websockets. The page is local, single-user, and small.

### Endpoints

- `GET /`
  - serves the HTML shell.
- `GET /app.js`
- `GET /styles.css`
- `GET /api/state`
  - returns staged settings, running settings, revision, dirty-vs-running diff, and
    service state.
- `GET /api/status`
  - returns the last cached status snapshot.
- `POST /api/apply`
  - body: proposed managed values + `base_revision`
  - response: success/failure, services touched, rollback status, and short logs.

### UI behavior

- Render from a static schema-driven form, not hard-coded DOM fragments per field.
- Group controls into:
  - Mining
  - Spam basics
  - Spam advanced
- Disable irrelevant fields when possible:
  - `SPAM_FANOUT_UTXOS` disabled if `SPAM_FANOUT_AUTO=true`
  - OUTPUT-specific settings visually marked as ignored in DATA/HYBRID mode
  - DATA/HYBRID-only settings visually marked as ignored when `SPAM_TX_DATA_MAX_BYTES=0`
- Show a pending state during apply and prevent concurrent applies.

### Security

Localhost-only binding is necessary but not sufficient. Because the panel is a browser
app that can rewrite `.env` and control Docker, add a minimal CSRF guard:

- generate a random token at startup,
- embed it in the served HTML,
- require it on every mutating request.

No authentication layer beyond that is needed in v1 because the port is bound to
`127.0.0.1` only.

## 12. Change 8: apply transaction, verification, and rollback

The apply path must be treated as a transaction:

1. acquire the apply lock,
2. re-read `.env` and confirm `base_revision`,
3. validate the proposed settings through the shared parsers,
4. compute the affected services by comparing proposed values against running values,
5. if the service set is empty, return success with `changed=false`,
6. back up the current `.env` contents in memory,
7. write the new canonical `.env`,
8. run `docker compose up -d --force-recreate <services...>`,
9. verify success,
10. on failure:
   - restore the old `.env`,
   - rerun the same compose recreate on the same service set,
   - report both the original failure and rollback result.

### Success verification

Do more than trust the compose exit code. After compose returns:

- inspect each targeted container by its pinned container name,
- confirm it is `running`,
- confirm its managed env values match what was just applied,
- wait a short stabilization window (for example 8-10 seconds),
- fail the apply if the container is restarting or exits during that window.

Also:

- if `btc-simnet-node1` was healthy before the apply, require `node1` RPC to still be
  reachable after it.

This catches the important failure class: "compose returned 0 but the restarted tool
then crashed immediately from a config/runtime issue."

## 13. What needs no behavior changes

- **Mining controller logic:** no mining behavior changes are needed. The existing
  bootstrap-resume behavior is exactly what makes live retuning safe.
- **Spammer logic:** no spam logic changes are required beyond sharing validation code.
- **Reorg simulator:** unaffected.
- **Node services:** unaffected in v1; the panel reads chain state from node1 but does
  not manage node lifecycle or node policy knobs.
- **Snapshot/restore flow:** unaffected; the panel works on top of the same persisted
  stack.

## 14. Documentation updates (same PR)

- **README.md**
  - add the `panel` profile to the profile table.
  - add one short "Dashboard / control panel" section with the URL and the purpose.
- **docs/RETUNING.md**
  - keep the manual flow, but add the panel as the UI path for the same operation.
- **docs/SETTINGS.md**
  - add `PANEL_WEB_PORT`.
  - note which settings are panel-managed in v1.
- **docs/NICE-TO-HAVE.md**
  - remove item #4 once shipped and renumber remaining items, per repo convention.
- **`.env.example` / `.env.full.example`**
  - add the optional `PANEL_WEB_PORT`.

## 15. Verification plan

### Automated

Add unit / handler tests for:

1. shared validation:
   - valid mining/spam settings parse successfully,
   - invalid combinations produce the same errors as the tool crates.
2. env file management:
   - no `.env` file -> defaults load,
   - managed keys are canonicalized into one block,
   - unmanaged lines are preserved verbatim,
   - stale revision rejects apply.
3. service mapping:
   - mining-only diff,
   - spam-only diff,
   - mixed diff,
   - no-op diff,
   - `FALLBACK_FEE` maps to spammer only.
4. apply handler:
   - compose success path,
   - compose failure triggers rollback,
   - post-start crash triggers rollback.

Mock process execution behind a small trait so tests do not require Docker.

### Manual, in order

1. Bring the stack up with the new panel:

   ```bash
   docker compose --profile panel up -d --build
   ```

   Open the panel and confirm it loads.

2. Status correctness:
   - panel height matches `bitcoin-cli getblockcount` on node1,
   - recent block tx counts roughly match the explorer / RPC,
   - mempool count changes as spammer cycles.

3. Mining-only apply:
   - change only `BLOCK_INTERVAL_MEAN_SECS` or `MINER_WEIGHTS`,
   - Apply,
   - confirm only `btc-simnet-mining-controller` is recreated,
   - chain continues, no re-bootstrap, cadence changes on later blocks.

4. Spammer-only apply:
   - change `SPAM_FILL_BLOCK_RATIO` or `ENABLE_SPAM`,
   - Apply,
   - confirm only `btc-simnet-spammer` is recreated,
   - mempool depth / block fullness changes accordingly.

5. Fee floor special case:
   - change only `FALLBACK_FEE`,
   - Apply,
   - confirm only the spammer is recreated,
   - floor-priced traffic moves to the new level,
   - nodes are not recreated.

6. Combined apply:
   - change one mining field and one spam field,
   - Apply,
   - confirm both tool services are recreated and the nodes stay up.

7. Invalid input:
   - set an impossible value such as `MINER_WEIGHTS=0,0` or manual fanout below the
     required minimum,
   - confirm the panel rejects it before touching `.env` or Docker.

8. Stale revision:
   - open two browser tabs,
   - apply from tab A,
   - attempt apply from stale tab B,
   - confirm tab B gets a conflict and must refresh.

9. Rollback:
   - simulate a compose failure with a mocked/faulty panel build or temporarily broken
     Docker access,
   - confirm `.env` returns to the previous contents and the old service config is
     restored.

10. Optional tools coexistence:
   - run with `--profile panel --profile mempool`,
   - apply a spam-only change,
   - confirm the mempool explorer stack is untouched.

## 16. Risks and edge cases

- **`docker.sock` is root-equivalent.** This is the main security fact. Keep the panel
  localhost-only, opt-in, and out of `all-tools`.
- **Compose/profile warnings.** Because the apply command only targets unprofiled base
  services and does not use `--remove-orphans`, extra profile services should survive.
  Some compose versions may still print warnings; treat them as non-fatal unless the
  command exits non-zero.
- **Status sampling cost.** `getrawmempool true` can be large under deep spam. Keep it
  on the slower cadence and cache the result.
- **Async + blocking mix.** `bitcoincore-rpc` and `std::process::Command` are blocking;
  forgetting `spawn_blocking` will make the UI feel hung under load.
- **Runtime drift outside the panel.** Manual `docker compose up --force-recreate` or
  direct `.env` edits can happen at any time. That is why the panel must show staged
  vs running values separately and use revision checks.
- **Future knob growth.** If more live-retunable settings are added later, update only
  the shared `SettingSpec` catalog and the UI schema; do not hand-edit mappings in
  three places.

## 17. Effort and change list

Medium-large. This is a new crate plus some shared-config refactoring, but it does not
require changes to the simnet's core behavior.

| File | Change |
| --- | --- |
| `Cargo.toml` | Add `crates/panel` to the workspace |
| `docker/tools.Dockerfile` | Add a panel final target with Docker CLI + compose plugin |
| `docker-compose.yml` | Add `btc-simnet-panel` service, `panel` profile, localhost port, repo bind mount, `docker.sock` mount |
| `crates/simchain-common/src/...` | Add shared live-retune setting catalog, defaults, and validators |
| `crates/mining-controller/src/config.rs` | Reuse shared live-retune validators/defaults |
| `crates/spammer/src/config.rs` | Reuse shared live-retune validators/defaults |
| `crates/panel/**` | New axum server, envfile logic, Docker/compose glue, embedded frontend assets, tests |
| `.env.example` | Add `PANEL_WEB_PORT` |
| `.env.full.example` | Add `PANEL_WEB_PORT` and short comment |
| `README.md` | Document `panel` profile and the browser UI |
| `docs/RETUNING.md` | Mention the panel as the UI equivalent of the manual flow |
| `docs/SETTINGS.md` | Document `PANEL_WEB_PORT` and panel-managed scope |
| `docs/NICE-TO-HAVE.md` | Remove item #4 once implemented |

## 18. Recommended implementation order

1. Extract shared live-retune validation into `simchain-common`.
2. Add the new panel crate with just `GET /api/state` and `GET /api/status`.
3. Add Docker inspect integration so the page can show staged vs running values.
4. Implement canonical `.env` rewriting and revision checks.
5. Implement `POST /api/apply` with rollback.
6. Add the compose service and Docker image target.
7. Finish the browser UI polish and the documentation updates.

That order keeps the highest-risk parts first: shared validation correctness and the
write/apply/rollback path.
