# Simchain Control Plane

The default localhost control plane combines the browser dashboard, versioned HTTP API,
MCP endpoint, durable job coordinator, and first-party CLI contract. It arrives with the
`minimal-api` profile and every richer one:

```bash
docker compose --profile minimal-api up -d --build
```

Open [http://localhost:8090/](http://localhost:8090/) (port: `CONTROL_PLANE_PORT`) to
watch chain state and manage live operations.

The control plane is itself profiled. A plain `docker compose up` is the chain-only
`minimal` shape and does not start it: the nodes bootstrap and mine, but nothing here is
available -- no API, no MCP, no dashboard, no jobs, no scenarios, and `simchainctl` has
nothing to talk to, since it is purely an API client. `--profile minimal-api` is the
smallest stack that includes it, and every richer profile does too.

With the control plane running, one capability still depends on the profile: **without
the network agents** (`minimal-api` omits them), partition and degrade submissions, and
any scenario containing those steps, are rejected with HTTP 503 `component_unavailable`
naming the profile to start. The check runs before the job is reserved, so a rejected
request leaves no job behind. Everything else is available.

## What It Owns

Mining and spam policy plus pause/resume use private worker APIs and never recreate
their containers. Reorgs, true shorter-chain rewinds, partitions, timed network
degradation, manual mine/burst actions, faucet funding, and scenarios are durable
server-side jobs under one scheduler.
Most mutations remain exclusive; a timed degradation may overlap a manual Mine job and
degradations on other nodes. Reorgs and partitions pause workers with expiring leases; namespace-local network
agents also heal on TTL expiry. Scenarios persist ordered steps, checkpoints, results,
and owned cleanup.

The control-plane image is intentionally narrow: it contains no Docker CLI, has no
Docker socket, drops all capabilities, uses a read-only root filesystem, and mounts only
its named state volume.

## Mutation coordinator

Dashboard, CLI, MCP, and direct HTTP clients all submit mutation jobs to the same
control-plane coordinator. At most one exclusive mutation runs at a time. Timed
degradations occupy a per-node lane and may overlap only a manual Mine job or a
degradation on another node. A second incompatible request is rejected; it is not queued
for later execution. The dashboard shows every active lane and disables conflicting
controls, while CLI/API/MCP callers receive the same busy/error response from the backend.

This is deliberate: queued chain mutations can become stale or unsafe after the active
job changes height, mempool contents, worker leases, faucet state, or network
impairments. For repeatable multi-step execution, put the ordered actions in one
scenario YAML and submit it as a single durable scenario job.

Idempotency keys are for retries, not queuing. Reusing the same key with the same
normalized request returns the existing accepted job; a different request must wait
until the coordinator is idle and be submitted again.

## Dashboard

The dashboard is the browser surface for the same operations exposed by the API and CLI:
status, live mining/spam retuning, manual worker pause/resume, durable jobs, faucet
funding, and local mempool.space health/linking when the `mempool` profile is active.
If a mining or spam worker is unreachable, its settings and manual state controls are
temporarily disabled and Apply waits for worker recovery. Unsaved field edits are
preserved, and the controls re-enable automatically after a successful status poll.
Bounded-action buttons are also disabled when one of their required workers, Bitcoin
nodes, or network agents is unreachable; the dashboard names the missing dependency.
Scenario submission remains available because dependencies are determined by its YAML.
The mining card contains separate **Mine blocks** and **Rewind chain** subpanels. Rewind
always targets all three nodes, shows the expected lower height in a confirmation, and
warns that disconnected transactions can return to node-local mempools.

Configuration applies never touch node chain state, and mixed mining/spam applies roll
back transactionally if a worker cannot accept or verify the new generation. Mining
cadence and weights apply at a scheduler safe point; spam hot changes apply between
cycles and structural changes reconcile a replacement engine before commit. See
[RETUNING.md](RETUNING.md).

The faucet funds up to 100 regtest destinations from one existing miner treasury. It
creates a real transaction with an actual fee of exactly 0 sat, then gives that tx a
fixed, miner-local 100 BTC virtual priority delta on node2 and node3 so the next normal
block includes it. The virtual amount is ordering metadata: it is not paid to the miner
or transferred to the recipient. This is a private regtest facility, not a public or
mainnet faucet.

## HTTP API

Everything the UI shows comes from the versioned localhost HTTP API. Common read routes:

```text
GET /api/v1/status
GET /api/v1/config
GET /api/v1/config/schema
GET /api/v1/scenario/schema
GET /api/v1/jobs
GET /api/v1/faucet
```

`/api/v1/config/schema` is the runtime-setting catalog; `/api/v1/scenario/schema` is the
declarative scenario language, described in [SCENARIOS.md](./SCENARIOS.md). Both are
generated from the same catalogs the server validates against, so neither can drift from
what the control plane actually accepts.

Mutating calls need a bearer token. The default zero-config stack uses
`simchain-control-dev-token`; if you override `CONTROL_PLANE_API_TOKEN`, pass the same
value with `--token` or `SIMCHAIN_CONTROL_TOKEN`:

```bash
token="${SIMCHAIN_CONTROL_TOKEN:-simchain-control-dev-token}"

curl -s localhost:8090/api/v1/status | jq .height

curl -s -X PATCH localhost:8090/api/v1/config \
  -H "Authorization: Bearer $token" \
  -H "Content-Type: application/json" \
  -d '{"settings": {"SPAM_FILL_BLOCK_RATIO": "0.5"}}'

curl -s -X PUT localhost:8090/api/v1/mining/state \
  -H "Authorization: Bearer $token" \
  -H "Content-Type: application/json" \
  -d '{"state": "paused"}'

job_id="$(curl -s -X POST localhost:8090/api/v1/jobs/reorg \
  -H "Authorization: Bearer $token" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: example-reorg-1" \
  -d '{"depth":3,"empty":true,"node":"node3"}' | jq -r .job_id)"
curl -s "localhost:8090/api/v1/jobs/$job_id/events?after=0" | jq .

curl -s -X POST localhost:8090/api/v1/jobs/rewind \
  -H "Authorization: Bearer $token" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: rewind-example-1" \
  -d '{"blocks":3}'
```

## MCP

The same operations are exposed over MCP (streamable HTTP) at
`http://localhost:8090/mcp`, so coding agents can inspect and retune the simnet
directly. Mutation tools include `start_reorg`, `rewind_chain`, `start_partition`, `start_degrade`,
`start_scenario`, `fund_addresses`, `get_faucet_status`, `get_faucet_transfer`,
`get_job`, `list_jobs`, and `abort_job` over the same coordinator and validation as
HTTP.

Register it in Claude Code with:

```bash
claude mcp add --transport http simchain-control-plane \
  "http://localhost:8090/mcp" \
  --header "Authorization: Bearer ${SIMCHAIN_CONTROL_TOKEN:-simchain-control-dev-token}"
```

For setup guidance, example prompts, and browser/auth troubleshooting, see
[MCP.md](MCP.md).

## CLI

`simchainctl` is a thin first-party HTTP client over the same control-plane API and
service operations:

```bash
cargo run -p simchainctl -- status
cargo run -p simchainctl -- status --watch
cargo run -p simchainctl -- config show --json
cargo run -p simchainctl -- config set BLOCK_INTERVAL_MEAN_SECS=12 SPAM_FILL_BLOCK_RATIO=3
cargo run -p simchainctl -- mining pause
cargo run -p simchainctl -- mining resume
cargo run -p simchainctl -- reorg start --depth 3 --empty --wait
cargo run -p simchainctl -- rewind --blocks 3 --wait
cargo run -p simchainctl -- partition start --node node3 --main-blocks 3 --isolated-blocks 5 --heal-delay-secs 15 --wait
cargo run -p simchainctl -- degrade start --node node2 --delay-ms 5000 --loss-pct 0 --seconds 30 --wait
cargo run -p simchainctl -- jobs list
cargo run -p simchainctl -- jobs watch JOB_ID --timeout 900
cargo run -p simchainctl -- jobs abort JOB_ID
cargo run -p simchainctl -- mine --node node2 --blocks 1 --wait
cargo run -p simchainctl -- spam prepare --node node2 --txs 10 --data-bytes 20000 --fee-rate-sat-vb 25 --wait
cargo run -p simchainctl -- spam burst --node node2 --txs 10 --data-bytes 20000 --fee-rate-sat-vb 25 --wait
cargo run -p simchainctl -- faucet --to bcrt1q...=1btc --to bcrt1p...=25000000sat --wait
cargo run -p simchainctl -- faucet status
cargo run -p simchainctl -- faucet transfer TXID --watch
```

`reorg start --wait` streams progress and exits `0` only after successful cleanup.
`rewind --wait` similarly waits for all three nodes to report the exact lower ancestor.
Unlike reorg, rewind mines no replacement blocks; it uses the internal node1 identity
only for node1's administrative RPC and rolls a partial operation back before releasing
its worker leases. A successful rewind job includes a structured
`electrs_reindex_may_be_required` advisory. It is conditional on an electrs-based
profile being active; recover a degraded bundled explorer with
`./scripts/recover-explorer.sh`.

Status keeps frontend reachability separate from index correctness. `explorer.reachable`
describes the mempool.space web frontend, `explorer.indexer_reachable` describes the
electrs HTTP API, and `explorer.synchronized` becomes true only when electrs reports the
same exact height and block hash as node1. The optional `recovery_command` is populated
for a reachable frontend backed by a stale or unavailable indexer.
`spam prepare` reads the same node, transaction count, and shape fields as
`spam burst`. It provisions the dedicated manual-burst branch pool and may mine
the minimum confirmation blocks without changing the mining controller's desired
paused/running state. A later burst checks this capacity without mining; if it is
insufficient, the failed job tells the operator to prepare it first. The optional
`--fee-rate-sat-vb` value must be supplied to both commands and selects the exact
manual-burst feerate. Omitting it preserves the live `SPAM_FEE` behavior.
Stable automation exit codes are:

| Code | Meaning |
|---:|---|
| `0` | Request/job succeeded |
| `1` | Server-reported operation or job failure |
| `2` | CLI usage or local file error |
| `3` | API unavailable or authentication failure |
| `4` | Wait timeout |
| `5` | Job aborted/interrupted or cleanup failed |

Job metadata and the most recent 100 summaries are stored in the
`btc-simnet-control-state` volume. A control-plane restart marks an unfinished job
interrupted and keeps each occupied lane locked until its owned network impairment is
healed and worker leases are confirmed clear.
