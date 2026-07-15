# Retuning a Live Chain

Settings consumed by the mining controller and the spammer can be changed **without restarting either worker**. The control plane applies mining settings at a scheduler safe point and spam settings at cooperative cycle boundaries. Structural spam changes build and reconcile a replacement engine before the old policy is replaced. The nodes keep running and the chain is preserved.

Three equivalent paths perform the same operation:

- **Manual** (below): edit `.env`, recreate the affected service(s).
- **Control-plane UI**: `docker compose --profile control-plane up -d`, then
  [http://localhost:8090/](http://localhost:8090/) — edit, Apply. The control plane validates
  first, stores durable desired state, mirrors `.env` for compatibility (managed keys are canonicalized into one
  `# Managed by simchain panel` block; your other lines are preserved), applies both
  worker policies in place, and rolls back automatically if any component rejects the apply.
- **Control-plane API / MCP**: `PATCH /api/v1/config` with the
  `.simchain-control/token` bearer token, or the `set_config` MCP tool at
  `http://localhost:8090/mcp` — same semantics, built for scripts and coding agents.

Mining can also be paused and resumed cooperatively with the dashboard controls,
`PUT /api/v1/mining/state`, the `set_mining_state` MCP tool, or:

```bash
cargo run -p simchainctl -- mining pause
cargo run -p simchainctl -- mining resume
```

A pause acknowledgement means any in-flight `generate` RPC and propagation check has
completed. Changing cadence, weights, or `MINING_RNG_SEED` wakes an interruptible wait;
the new generation takes effect at the next scheduler boundary. A seed change resets
the worker RNG and miner-alternation toggle deterministically.

Spam has matching controls through `PUT /api/v1/spam/state`, the `set_spam_state`
MCP tool, the dashboard, or:

```bash
cargo run -p simchainctl -- spam pause
cargo run -p simchainctl -- spam resume
```

A spam pause is acknowledged only after already-submitted work reaches a consistent
boundary. `ENABLE_SPAM=false` keeps the worker resident in its `disabled` phase, so it
can be inspected and re-enabled without a container restart.

## Steps

1. Edit `.env`. For example:
   - Mining cadence: `BLOCK_INTERVAL_MEAN_SECS`, `BLOCK_INTERVAL_MODE`,
     `BLOCK_INTERVAL_MIN_SECS`/`MAX_SECS`, `MINER_WEIGHTS` (mining controller).
   - Fee floor and block filling: `FALLBACK_FEE`, `SPAM_FILL_BLOCK_RATIO`,
     `SPAM_FLOOR_POOL_TXS`, `SPAM_TX_DATA_MAX_BYTES`/`MIN_BYTES`, `ENABLE_SPAM`,
     `ENABLE_SPAM_REPLACES` (spammer).

2. Recreate only the affected service(s), both:

   ```bash
   docker compose up -d --force-recreate btc-simnet-mining-controller btc-simnet-spammer
   ```

   or just the one you changed:

   ```bash
   docker compose up -d --force-recreate btc-simnet-spammer
   ```

## Safety & Behavior

The manual fallback is safe mid-run because `--force-recreate` replaces only the named services; the node dependencies remain running, so the chain, wallets, and mempool survive. The control-plane path does not replace either worker and reports desired/effective generations plus exact safe-point phases.

## Caveats

- Settings consumed by the **nodes** (`BTC_IMAGE`, host ports, `MIN_RELAY_TX_FEE`,
  ZMQ ports, ...) do require recreating the nodes, and node containers keep the chain
  in their filesystem, so that resets the chain: use a full
  `docker compose --profile all-tools down` / `up`.
- `FALLBACK_FEE` is shared: the spammer prices its floor fills with it, and the nodes
  take it as `-fallbackfee` (wallet-side fallback). A spam engine rebuild moves the
  spam fee floor immediately; wallet mode also sets wallet `paytxfee`. The nodes keep
  their boot fallback until a full restart, which is usually irrelevant.
