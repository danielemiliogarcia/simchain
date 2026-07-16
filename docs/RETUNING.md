# Retuning a Live Chain

Mining and spam policy can change without restarting nodes or either resident worker.
The control plane owns durable desired state in the `btc-simnet-control-state` Docker
volume, applies a complete typed policy through private worker APIs, verifies the
effective generation, and restores the prior runtime policy if a multi-worker
transaction fails.

Start the ordinary stack and open [http://localhost:8090/](http://localhost:8090/):

```bash
docker compose up -d --build
```

The dashboard, CLI, HTTP API, and MCP tool are adapters over the same operation. For
example:

```bash
cargo run -p simchainctl -- config show
cargo run -p simchainctl -- config set \
  BLOCK_INTERVAL_MEAN_SECS=12 SPAM_FILL_BLOCK_RATIO=3
cargo run -p simchainctl -- status --watch
```

Use `--base-generation N` with `config set` for compare-and-swap behavior. A stale
editor receives `409 stale_revision` and does not mutate either worker.

The equivalent HTTP request is:

```bash
token="${SIMCHAIN_CONTROL_TOKEN:-simchain-control-dev-token}"
generation="$(curl -s localhost:8090/api/v1/config | jq .generation)"
curl -s -X PATCH localhost:8090/api/v1/config \
  -H "Authorization: Bearer $token" \
  -H 'Content-Type: application/json' \
  -d "{\"settings\":{\"FALLBACK_FEE\":\"0.0002\"},\"base_generation\":$generation}"
```

MCP exposes the same operation as `set_config` at `http://localhost:8090/mcp`.

## Safe-point behavior

- Mining cadence, bounds, weights, and RNG seed apply at a scheduler safe point. The
  interruptible scheduler wakes immediately for a new generation. Changing
  `MINING_RNG_SEED` reinitializes the RNG and alternation state deterministically.
- Spam target/count changes apply between cooperative cycle boundaries. Fee, engine,
  data shape, and fanout changes build and reconcile a replacement engine before the
  old policy is discarded.
- `ENABLE_SPAM=false` leaves the spam worker resident in `disabled`, so status and live
  re-enable remain available.

Pause and resume are separate durable desired-state controls:

```bash
cargo run -p simchainctl -- mining pause
cargo run -p simchainctl -- mining resume
cargo run -p simchainctl -- spam pause
cargo run -p simchainctl -- spam resume
```

A pause acknowledgement means the worker reached its documented safe point. Job-owned
pause leases remain independent of manual desired state, so releasing a job cannot
resume a manually paused worker.

## Configuration ownership

`.env` is Compose boot input. It still owns infrastructure such as images, credentials,
ports, RPC endpoints, node policy, and the initial mining/spam policy used when no
control-state file exists. The control plane never rewrites `.env`; after first boot,
runtime desired policy lives only in the `btc-simnet-control-state` volume.
The same private directory holds the mutation and process-instance locks, so a second
control-plane process cannot coordinate jobs against the same state concurrently.

To intentionally reset runtime desired policy to new boot values, stop the stack,
remove only the control-state volume, edit `.env`, and start again:

```bash
docker compose down
docker volume rm btc-simnet-control-state
```

Removing that volume is an explicit reset and also resets its generation and job
history. It does not remove the node chain volumes.

Node settings such as `BTC_IMAGE`, host ports, `MIN_RELAY_TX_FEE`, ZMQ ports, and
`BLOCK_RESERVED_WEIGHT` are boot-only. Change those through Compose with the normal
operational care for node restarts; they are deliberately absent from the live schema.

`FALLBACK_FEE` is shared at boot: nodes use it for wallet fallback, while the spammer
uses it as the live fee floor. A runtime change updates the spam engine (and wallet
`paytxfee` in wallet mode) but does not rewrite a running node's boot fallback fee.
