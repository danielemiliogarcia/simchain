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
  -d "{\"settings\":{\"SPAM_FEE\":\"0.0002\"},\"base_generation\":$generation}"
```

MCP exposes the same operation as `set_config` at `http://localhost:8090/mcp`.

## Safe-point behavior

- Mining cadence, bounds, weights, and RNG seed apply at a scheduler safe point. The
  interruptible scheduler wakes immediately for a new generation. Changing
  `MINING_RNG_SEED` reinitializes the RNG and alternation state deterministically.
- Spam settings apply between cooperative transaction or cycle boundaries. Target and
  count changes recalculate the next workload; fee and data/output shape update the
  resident raw engines in place while preserving their tracked UTXOs and floor pools.
- A DATA/HYBRID fill-ratio increase schedules one same-height mempool-deficit catch-up.
  Fanout uses a minimum capacity of `ratio x 10` and a preferred target of `ratio x 15`,
  so existing headroom keeps sending while extra branches confirm in the background.
  A capacity-only target increase also wakes the worker immediately so funding or fanout
  can be submitted without waiting for another block; unchanged fixed/small transaction
  counts are suppressed by the catch-up delta. Bulk DATA traffic is submitted before
  floor-pool maintenance so refilling thousands of small floor transactions cannot delay
  establishment of the requested backlog. Ratios below `1` intentionally stop
  replenishing the floor pool so partial-block targets remain observable.
- `ENABLE_SPAM=false` leaves both the worker and its healthy raw engines resident in
  `disabled`. Re-enable resumes their state without a scan unless a reorg or another
  recovery event marked it stale.
- Full reconciliation is reserved for process startup, snapshot restore, chain mutation,
  an expired mutation lease, or detected stale outpoints. Dashboard capacity status is
  separate from the effective policy generation. `branch provisioning` and `floor pool
  provisioning` explicitly show background work even when capacity is temporarily
  degraded. The startup scan is initialization and leaves the process-lifetime
  `recoveries` counter at zero. That counter increments only when a previously healthy
  engine is reconstructed or dirty runtime state is reconciled.

Confirmed branch outputs that have become too small for the current transaction shape
remain tracked but are not automatically consolidated. This can leave undersized UTXOs
on the spammer's dedicated deterministic addresses during very long runs. Automatic
sweeping would add consolidation traffic and fee pressure not requested by the active
scenario; startup/reorg reconciliation rediscovers these outputs but deliberately does
not spend them.

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

`FALLBACK_FEE` is boot-only too: it sets the nodes' wallet estimator fallback and
appears in the dashboard as a read-only label. The live fee floor is the separate
`SPAM_FEE` changes the raw engine's transaction shape in place at a safe boundary
without touching the running nodes or discarding tracked transaction state.
A legacy `.env` that sets only `FALLBACK_FEE` still seeds `SPAM_FEE` at first boot.
The value is BTC/kvB (`0.001` = 100 sat/vB). Raising it also raises the capital needed
by every DATA branch; an unaffordable fee, payload, and fanout combination reports
`capacity_degraded` and cannot provision its way back to the requested capacity.
It can also consume spendable miner treasury below `FAUCET_RESERVE_BTC`, leaving no
faucet availability until the setting is reduced, funds are added, or mined fees
complete their 100-block maturity.
