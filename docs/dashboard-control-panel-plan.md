# Implementation plan: dashboard / control panel

## Status: READY TO IMPLEMENT (written 2026-07-10, expanded 2026-07-12: API-first + MCP)

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
- exposes every read and mutation as a versioned localhost HTTP API (`/api/v1/`),
  consumed by the browser UI and by programmatic clients alike.
- exposes the same operations over MCP (streamable HTTP on the same port) so coding
  agents can inspect and retune the simnet without scraping the UI.

The first supported workflow is:

```bash
docker compose --profile panel up -d
```

Then visit `http://localhost:8090` in a browser and:

1. inspect the current chain state,
2. change mining cadence / miner weights / spam settings,
3. click Apply,
4. watch the effect on the live mempool and block stream.

The second supported workflow is headless: an agent (or `curl`) hits the same
versioned API — or connects over MCP — to read chain status, discover the knob
schema, and apply retunes programmatically.

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
3. The browser is a pure API client: it polls the versioned JSON endpoints and
   renders from them. No data reaches the page any other way (the only exception is
   the API token injected into the HTML shell, see §11).
4. An apply request validates the proposed settings, rewrites `.env` atomically,
   recreates only the necessary service(s), verifies the result, and rolls back on
   failure.
5. All operations live in one transport-agnostic service layer (`service.rs`); the
   HTTP handlers and the MCP tools are both thin adapters over it, so the two
   surfaces cannot drift.
6. The same `axum` server mounts an MCP endpoint (streamable HTTP) at `/mcp`,
   exposing the service layer as MCP tools for agents (§13).

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
    service.rs
    api.rs
    mcp.rs
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
- `service.rs`
  - transport-agnostic operations layer: `get_status()`, `get_settings()`,
    `get_schema()`, `apply_settings()`. Owns the apply lock and the partial-merge +
    revision semantics, and defines the shared response/error types. The only module
    `api.rs` and `mcp.rs` are allowed to call.
- `api.rs`
  - HTTP handlers and JSON serialization; thin adapter over `service.rs`.
- `mcp.rs`
  - MCP tool definitions and input schemas; thin adapter over `service.rs` (§13).

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
- `rmcp` (the official Rust MCP SDK), features `server` +
  `transport-streamable-http-server`; it integrates with the existing `axum`/`tokio`
  stack as a nested router service
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
  - `PANEL_API_TOKEN=${PANEL_API_TOKEN:-}` (optional passthrough; empty means
    auto-generate, see §11 Security)
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

## 11. Change 7: HTTP API (v1) and browser UI

Prefer plain polling over SSE or websockets. The page is local, single-user, and small.

The API is versioned from day one because it has two consumer classes — the browser UI
and programmatic clients (agents, MCP, `curl`) — with independent lifecycles. All JSON
routes live under `/api/v1/`.

### Endpoints

- `GET /`
  - serves the HTML shell (with the API token injected, see Security below).
- `GET /app.js`
- `GET /styles.css`
- `GET /api/v1/state`
  - returns staged settings, running settings, revision, dirty-vs-running diff, and
    service state.
- `GET /api/v1/status`
  - returns the last cached status snapshot, including `last_updated` and staleness
    info.
- `GET /api/v1/schema`
  - returns the `SettingSpec` catalog from §5 as JSON: names, types, defaults,
    groups, restart scope, help text, and validation bounds where expressible.
    The browser renders the form from it; agents use it to discover the knobs.
- `POST /api/v1/apply`
  - body: a **partial** map of managed keys to proposed values, plus optional
    `base_revision`.
  - semantics: proposed values are merged over the current staged values under the
    apply lock, then the full merged set is validated and applied. If
    `base_revision` is present and stale, respond `409 Conflict`. If absent, the
    merge runs against whatever is current — convenient for agents that just want
    "set `SPAM_FILL_BLOCK_RATIO` to 0.5" without a read-fetch dance.
  - response: success/failure, services touched, rollback status, and short logs.

The browser UI always sends `base_revision` (it has one from `GET /api/v1/state`), so
the tab-vs-tab conflict behavior in §12 is unchanged. Partial merge is a superset of
full-set semantics, not a second code path.

### API contract

- Every response is JSON with a stable shape defined in `service.rs`; the HTTP and
  MCP adapters serialize the same types.
- Errors use one envelope:

  ```json
  { "error": { "code": "validation_failed", "message": "...", "details": [ ... ] } }
  ```

- `code` is a small closed enum — `validation_failed`, `stale_revision`,
  `apply_in_progress`, `compose_failed`, `rollback_failed`, `rpc_unavailable`,
  `unauthorized` — because agents branch on codes, not prose.
- Field-level validation errors name the exact env var they refer to.

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
app that can rewrite `.env` and control Docker, mutating requests need a token. A
CSRF-token-embedded-in-HTML-only design would lock out non-browser clients, so v1 uses
one bearer token serving both consumer classes:

- token source: `PANEL_API_TOKEN` env var if set; otherwise generated randomly at
  startup.
- persistence: written to `${SIMCHAIN_REPO_ROOT}/.panel-token` (mode `0600`); the repo
  bind mount makes it readable from the host, so agents and scripts can pick it up.
  Add `.panel-token` to `.gitignore`.
- browser path: the token is injected into the served HTML and sent by `app.js` as
  `Authorization: Bearer <token>`. This doubles as the CSRF guard — a cross-site
  request cannot read the page to obtain it.
- programmatic path: read `.panel-token` (or the env var) and send the same header.
- enforcement: required on `POST /api/v1/apply` and on the whole `/mcp` endpoint
  (MCP sessions can reach the mutating tool). Read-only `GET /api/v1/*` stays
  tokenless for `curl`-friendliness: it is localhost-only and discloses nothing that
  `.env` and `docker inspect` do not already expose locally.

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

## 13. Change 9: MCP interface for agents

Expose the same service layer over MCP so coding agents (Claude Code and any other
MCP client) can inspect and retune the simnet directly — no HTML scraping, no bespoke
client code.

### Transport

Use the official Rust MCP SDK (`rmcp`) with the **streamable HTTP** server transport,
mounted into the existing `axum` router at `/mcp`:

- no second port, no second process, no stdio proxy;
- the MCP tools call `service.rs` in-process — the exact code path the HTTP API uses,
  same apply lock, same partial-merge and revision semantics;
- any MCP client that speaks streamable HTTP connects to
  `http://localhost:${PANEL_WEB_PORT:-8090}/mcp` with the bearer token.

A stdio transport is explicitly out of scope for v1: the panel binary lives in a
container, so stdio would require a host binary or a docker-exec shim. If a
stdio-only client matters later, an off-the-shelf HTTP-to-stdio proxy (e.g.
`mcp-remote`) solves it with zero repo changes.

Client registration example (Claude Code):

```bash
claude mcp add --transport http simchain-panel \
  "http://localhost:8090/mcp" \
  --header "Authorization: Bearer $(cat .panel-token)"
```

### Tools

Four tools, mapping 1:1 onto the service layer (and therefore onto the §11
endpoints):

| Tool | Maps to | Annotations |
| --- | --- | --- |
| `get_status` | `GET /api/v1/status` | read-only |
| `get_settings` | `GET /api/v1/state` | read-only |
| `get_setting_schema` | `GET /api/v1/schema` | read-only |
| `apply_settings` | `POST /api/v1/apply` | mutating, not idempotent |

`apply_settings` input:

```json
{ "settings": { "<ENV_VAR>": "<value>", "...": "..." }, "base_revision": "optional" }
```

Same partial-merge semantics as the HTTP endpoint. The tool description must state
that it rewrites `.env` and recreates tool containers, and must carry the
`FALLBACK_FEE` node-restart caveat from §9 — the tool description is the only "UI
warning" an agent ever sees.

Tool results return the same JSON payloads as the HTTP API (serialized into the text
content), so everything documented for the API holds verbatim for MCP, including the
error-code envelope.

### What MCP does not get

- No tool that recreates nodes, runs arbitrary compose commands, or edits unmanaged
  env keys. The MCP surface is exactly the panel surface — the §5 exclusion list
  binds here too.
- No MCP resources or prompts in v1; the schema tool covers discovery. Add them later
  only if a real agent workflow wants them.

## 14. What needs no behavior changes

- **Mining controller logic:** no mining behavior changes are needed. The existing
  bootstrap-resume behavior is exactly what makes live retuning safe.
- **Spammer logic:** no spam logic changes are required beyond sharing validation code.
- **Reorg simulator:** unaffected.
- **Node services:** unaffected in v1; the panel reads chain state from node1 but does
  not manage node lifecycle or node policy knobs.
- **Snapshot/restore flow:** unaffected; the panel works on top of the same persisted
  stack.

## 15. Documentation updates (same PR)

- **README.md**
  - add the `panel` profile to the profile table.
  - add one short "Dashboard / control panel" section with the URL and the purpose.
  - document the HTTP API base path, the token file, the MCP endpoint, and the
    `claude mcp add` one-liner from §13.
- **docs/RETUNING.md**
  - keep the manual flow, but add the panel (UI, API, and MCP) as equivalent paths
    for the same operation.
- **docs/SETTINGS.md**
  - add `PANEL_WEB_PORT`.
  - add `PANEL_API_TOKEN` and the `.panel-token` file.
  - note which settings are panel-managed in v1.
- **docs/NICE-TO-HAVE.md**
  - remove item #4 once shipped and renumber remaining items, per repo convention.
- **`.env.example` / `.env.full.example`**
  - add the optional `PANEL_WEB_PORT`.

## 16. Verification plan

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
5. API contract:
   - versioned routes respond; unknown `/api/` paths return 404,
   - `POST /api/v1/apply` without the bearer token is rejected with
     `unauthorized`; with the token it proceeds,
   - partial apply merges over staged values and leaves omitted keys untouched,
   - stale `base_revision` returns the `stale_revision` error code; omitted
     `base_revision` merges against current.
6. MCP:
   - an in-process `rmcp` client lists exactly the four tools,
   - `get_settings` returns the same payload as `GET /api/v1/state`,
   - a session without the bearer token cannot reach the tools,
   - `apply_settings` with an invalid value returns `validation_failed` without
     touching `.env` (mocked executor).

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

11. API + MCP end-to-end:
   - `curl` the read endpoints and confirm they match the UI,
   - `curl -X POST /api/v1/apply` with the token and a single-key change; confirm
     the same recreate behavior as the browser Apply,
   - register the MCP endpoint with `claude mcp add` as in §13,
   - from the agent: read status, read the schema, apply a spam-only change, and
     confirm the same container-recreate behavior.

## 17. Risks and edge cases

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
  three places. The API `/api/v1/schema` endpoint and the MCP schema tool pick new
  knobs up automatically from the catalog.
- **MCP exposes a mutating tool to agents.** Blast radius is bounded by design: the
  tool surface equals the panel surface, the §5 exclusion list applies, and the shared
  validators reject anything the tool containers would reject on restart. Do not add
  "convenience" tools that shell out or bypass `service.rs`.
- **Token handling.** `.panel-token` lands in the host working tree via the bind
  mount; it must be gitignored and written mode `0600`. Leaking it is equivalent to
  granting panel access, which is docker.sock-adjacent — never log its value.

## 18. Effort and change list

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
| `crates/panel/**` | New axum server, service layer, versioned HTTP API, MCP server (`rmcp`), envfile logic, Docker/compose glue, embedded frontend assets, tests |
| `.gitignore` | Add `.panel-token` |
| `.env.example` | Add `PANEL_WEB_PORT` |
| `.env.full.example` | Add `PANEL_WEB_PORT`, optional `PANEL_API_TOKEN`, short comments |
| `README.md` | Document `panel` profile, the browser UI, the HTTP API, and the MCP endpoint |
| `docs/RETUNING.md` | Mention the panel (UI/API/MCP) as equivalents of the manual flow |
| `docs/SETTINGS.md` | Document `PANEL_WEB_PORT`, `PANEL_API_TOKEN`, and panel-managed scope |
| `docs/NICE-TO-HAVE.md` | Remove item #4 once implemented |

## 19. Recommended implementation order

1. Extract shared live-retune validation into `simchain-common`.
2. Add the new panel crate with the `service.rs` layer and just `GET /api/v1/state`,
   `GET /api/v1/status`, and `GET /api/v1/schema`.
3. Add Docker inspect integration so the page can show staged vs running values.
4. Implement canonical `.env` rewriting and revision checks.
5. Implement apply (partial merge + rollback) in the service layer and expose it as
   `POST /api/v1/apply` with the bearer-token guard.
6. Mount the MCP endpoint (`/mcp`) over the finished service layer.
7. Add the compose service and Docker image target.
8. Finish the browser UI polish and the documentation updates.

That order keeps the highest-risk parts first: shared validation correctness and the
write/apply/rollback path.

## 20. Review findings (2026-07-12)

The overall architecture is workable, but the following issues should be corrected
before implementation. The first six affect core apply behavior and can make a valid
retune fail, target the wrong Compose project, or leave the host checkout in an
unusable state.

### Finding 1 — Critical: the panel's startup environment can override the rewritten `.env`

The plan tells `main.rs` to load `.env` with `dotenvy` and later spawns `docker compose`.
If loading populates the panel process environment, every managed value present at
startup is inherited by the child process. Compose gives shell environment variables
precedence over the project `.env`, so after the panel rewrites the file, the recreate
can still receive the old values. Verification will then fail and rollback, making live
retuning appear completely broken for keys that existed when the panel started.

Change the implementation as follows:

- Parse `/workspace/.env` into an in-memory map; do not install its values into the
  long-lived panel process environment.
- Read panel bootstrap settings and RPC credentials explicitly from process env plus
  that map rather than relying on a global `dotenvy::dotenv()` side effect.
- Invoke Compose with `--env-file /workspace/.env` (when the file exists) and remove
  every managed key and recognized legacy alias from the child command environment.
  Keep unrelated variables such as Docker connection settings and
  `COMPOSE_PROJECT_NAME`.
- Add an executor test where the process environment contains an old managed value,
  the file contains a new value, and the generated Compose invocation resolves the new
  value.

### Finding 2 — Critical: nested Compose can select a different project

The proposed command uses `--project-directory /workspace`. Unless a project name is
also preserved, Compose derives `workspace`, while a normal host invocation for this
checkout derives `simchain` (or a user-supplied `-p` value). With globally pinned
`container_name` values this can fail with name conflicts instead of recreating the
existing services. The existing scenario service already accounts for this exact
container-path problem by passing `COMPOSE_PROJECT_NAME` into the container.

Pass `COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-simchain}` to the panel service and
make the nested command use the preserved value explicitly with `-p`. Do not derive the
project from `/workspace`. Add a test using a non-default project name and confirm that
the generated command retains it.

### Finding 3 — High: disabling spam fails the proposed success check

`ENABLE_SPAM=false` is a supported live retune, but the spammer intentionally logs that
there is nothing to do and exits with code 0. Its Compose restart policy is
`on-failure`, so it remains cleanly exited. Section 12 currently requires every targeted
container to be `running`; consequently a valid disable operation will always be
classified as a failed apply and rolled back.

Make verification setting-aware:

- With `ENABLE_SPAM=true`, require the spammer to remain running and non-restarting
  through the stabilization window.
- With `ENABLE_SPAM=false`, require the recreated container to reach `exited` with
  exit code 0, not be restarting, and contain the proposed managed environment.
- Keep the mining controller's expected state as running.
- Add both disable and re-enable apply tests, including the manual verification list.

### Finding 4 — High: `FALLBACK_FEE` is not live-retunable in the node-wallet engine

The plan says recreating only the spammer changes the fee floor immediately. That is
true for the raw engine, which constructs fees from the spammer's `FALLBACK_FEE`, but
not for `USE_RAW_TX_SPAM=false`: the node-wallet engine calls `sendtoaddress`/`sendmany`
without setting a fee rate, so the already-running nodes continue using their old
`-fallbackfee`. Recreating the spammer alone does not change wallet-engine transaction
fees.

To keep `FALLBACK_FEE` a real panel knob in both engines, change the spammer's
node-wallet startup path to call `settxfee` on both miner wallets using the validated
`FALLBACK_FEE` before sending transactions. Treat failure to set either wallet as a
startup error so apply verification can roll back. Also validate the requested fee
against the running nodes' relay/mempool minimum; a floor below that minimum produces
transactions the nodes reject. If that behavior change is not desired, the alternative
is to mark `FALLBACK_FEE` non-live/read-only whenever the node-wallet engine is selected
and require a manual node recreation. The UI, API schema, MCP description, non-goals,
and tests must all describe whichever behavior is chosen.

### Finding 5 — High: root-owned bind-mount writes break host access and token use

The panel image has no `user` setting and needs the Docker socket, so it will normally
run as root. A new `.panel-token` written as mode `0600` will therefore be owned by root
on the host bind mount and cannot be read by the ordinary host user; the documented
`cat .panel-token` command fails. Atomic replacement of `.env` can likewise turn a
user-owned file into a root-owned file, preventing later manual retuning.

The file layer must preserve host ownership and modes:

- For an existing `.env`, copy its uid, gid, and permission bits to the temp file before
  rename. For a new `.env` and for `.panel-token`, use the bind-mounted repository
  directory's uid/gid; keep the token at `0600`.
- When `PANEL_API_TOKEN` is absent, reuse a valid existing `.panel-token` instead of
  generating a new token on every panel restart. Write a newly generated token
  atomically and never log it.
- Track whether `.env` originally existed. Rollback must remove a newly created file,
  not restore it as an empty file, and must restore original metadata for an existing
  file.
- Add ownership/mode, token-restart, and initially-absent rollback tests on Unix.

Running the container as the host uid/gid is also viable, but then the Compose design
must explicitly grant that user access to the host Docker socket (whose gid is not
portable). Do not solve token readability by weakening it to `0644`.

### Finding 6 — High: the no-op branch can leave `.env` permanently dirty

Section 12 returns before writing when proposed values equal the running container
values. That comparison answers whether a service must be recreated, not whether the
file must change. For example, if a user manually stages a bad/unwanted value in `.env`
and then uses the panel to change it back to the currently running value, the service
set is empty and the plan returns without fixing `.env`. The UI continues to show the
staged/running mismatch, and the unwanted value will take effect on a future manual
recreate.

Compute two independent changes under the apply lock:

- `file_changed`: canonical proposed settings differ from current staged settings;
- `services_to_recreate`: typed effective proposed settings differ from typed running
  settings for each service.

Write `.env` whenever `file_changed` is true. Recreate only
`services_to_recreate`. Return a no-op only when both are empty, and report file changes
separately from restarted services in the API/MCP result. Compare typed/canonical
values, not raw strings such as `2` versus `2.0`. Add file-only, runtime-only, and true
no-op tests.

### Finding 7 — Medium: legacy spam aliases are lost or can silently change behavior

The current spammer still honors `SPAM_TXS_PER_BLOCK`,
`SPAM_PER_MINER_PER_BLOCK`, and `SPAM_TX_DATA_BYTES` when their canonical replacements
are absent. The proposed managed catalog contains only the new names and preserves all
other lines verbatim. An old `.env` will therefore be displayed using panel defaults
rather than its effective legacy values; applying an unrelated setting appends the new
canonical defaults, which take precedence and silently changes the spam configuration.

Treat legacy aliases as recognized migration inputs even though they are not exposed as
editable schema fields. The read path must resolve the same precedence and conversion
rules as the spammer. On the first successful write, remove the legacy aliases and emit
only their canonical equivalents (including the per-miner-to-total conversion). Add
fixtures for every alias, for canonical-plus-alias precedence, and for an unrelated
partial apply against a legacy file.

### Finding 8 — Medium: a "simple lock file" is underspecified and can deadlock future applies

Merely creating a lock pathname is not sufficient mutual exclusion, and using
`create_new` without a robust stale-lock protocol can block forever after a crash.
Use an OS-backed advisory exclusive lock held by an open file descriptor for the entire
read/write/recreate/verify/rollback transaction (for example via `fs2` or `flock`). The
kernel then releases it if the process dies. Acquire it with non-blocking semantics so
the API can return `apply_in_progress`, and test contention between two independent
service instances. The in-process mutex remains useful for coordinating tasks within
one server.

### Finding 9 — Low: ten displayed blocks do not provide ten cadence deltas

Ten block records contain only nine adjacent timestamp deltas. To report a rolling
cadence over the last ten deltas while displaying ten blocks, fetch the latest eleven
block headers/records, calculate ten deltas, and omit the oldest block from the displayed
list. Near genesis, use all available deltas and return the sample count so consumers do
not mistake a short sample for ten observations.

### Verification additions required by these findings

In addition to the tests already listed in §16, the end-to-end apply test should assert
the Compose project label/name, resolved environment values, expected state for a
disabled spammer, and host readability/ownership of both `.env` and `.panel-token`.
The fee test should cover both raw and node-wallet engines. The status test should assert
that a ten-delta cadence uses eleven blocks when enough history exists.

## 21. Post-implementation review findings and resolutions (2026-07-12)

The implementation review found eight additional issues. They were resolved as follows:

1. **Rollback used the old file rather than the old runtime.** Apply now snapshots the
   touched containers before mutation and rollback recreates them with their exact
   pre-apply managed environment. Containers that were originally absent are removed,
   and the restored runtime is inspected and verified before rollback is reported as
   successful.
2. **The loopback listener was vulnerable to DNS rebinding.** Every route now rejects
   non-loopback `Host` headers before serving the token-bearing page. `localhost`, IPv4
   loopback, and IPv6 loopback authorities are accepted.
3. **The browser had stored-XSS and script-injection paths.** Staged errors and warnings
   are rendered with `textContent`, and the token is injected as a JSON string literal
   with HTML-significant characters escaped.
4. **Manual `.env` edits could be overwritten during apply.** The panel re-reads the file
   after recreate/verification and treats an unexpected revision as a conflict. Rollback
   restores the old file only when its compare-and-swap revision still matches the
   panel-written revision, preserving newer external edits.
5. **Empty optional mining bounds acquired Compose defaults.** The Compose expressions
   now distinguish unset from explicitly empty values, and the staged/env-file layers
   preserve empty optional values as unbounded.
6. **Invalid dormant spam values prevented disabling spam.** When `ENABLE_SPAM=false`,
   valid mining settings are retained and invalid ignored spam settings are reset to
   catalog defaults, matching the spammer's early clean-exit behavior.
7. **Docker/status errors were hidden or cleared by unrelated success.** Docker inspect
   failures now propagate, and RPC, Docker, and slow-sample errors are tracked
   independently and aggregated without one sampler clearing another's failure.
8. **The implementation did not pass the formatting gate.** The workspace is formatted
   with `cargo fmt --all`; the CI-equivalent build, Clippy, format check, and serial test
   suite are required before handoff.
