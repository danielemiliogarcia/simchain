# Simchain Control Plane

The default localhost control plane combines the browser dashboard, versioned HTTP API,
MCP endpoint, durable job coordinator, and first-party CLI contract. It is part of the
ordinary Compose stack:

```bash
docker compose up -d --build
```

Open [http://localhost:8090/](http://localhost:8090/) (port: `CONTROL_PLANE_PORT`) to
watch chain state and manage live operations.

## What It Owns

Mining and spam policy plus pause/resume use private worker APIs and never recreate
their containers. Reorgs, partitions, timed network degradation, manual mine/burst
actions, faucet funding, and scenarios are durable server-side jobs under one mutation
lock. Reorgs and partitions pause workers with expiring leases; namespace-local network
agents also heal on TTL expiry. Scenarios persist ordered steps, checkpoints, results,
and owned cleanup.

The control-plane image is intentionally narrow: it contains no Docker CLI, has no
Docker socket, drops all capabilities, uses a read-only root filesystem, and mounts only
its named state volume.

## Mutation coordinator

Dashboard, CLI, MCP, and direct HTTP clients all submit mutation jobs to the same
control-plane coordinator. At most one mutation job runs at a time. If a reorg,
scenario, manual mine, spam burst, partition, degradation, or faucet job already owns
the coordinator, a second incompatible request is rejected; it is not queued for later
execution. The dashboard shows the active job banner and disables conflicting controls,
while CLI/API/MCP callers receive the same busy/error response from the backend.

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
GET /api/v1/jobs
GET /api/v1/faucet
```

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
```

## MCP

The same operations are exposed over MCP (streamable HTTP) at
`http://localhost:8090/mcp`, so coding agents can inspect and retune the simnet
directly. Mutation tools include `start_reorg`, `start_partition`, `start_degrade`,
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
cargo run -p simchainctl -- partition start --node node3 --main-blocks 3 --isolated-blocks 4 --wait
cargo run -p simchainctl -- degrade start --node node3 --delay-ms 500 --loss-pct 1 --seconds 60 --wait
cargo run -p simchainctl -- jobs list
cargo run -p simchainctl -- jobs watch JOB_ID --timeout 900
cargo run -p simchainctl -- jobs abort JOB_ID
cargo run -p simchainctl -- mine --node node2 --blocks 1 --wait
cargo run -p simchainctl -- spam burst --node node2 --txs 100 --outputs-per-tx 25 --wait
cargo run -p simchainctl -- faucet --to bcrt1q...=1btc --to bcrt1p...=25000000sat --wait
cargo run -p simchainctl -- faucet status
cargo run -p simchainctl -- faucet transfer TXID --watch
```

`reorg start --wait` streams progress and exits `0` only after successful cleanup.
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
interrupted and keeps the coordinator locked until any network impairment is healed,
convergence is witnessed, and worker leases are confirmed clear.
