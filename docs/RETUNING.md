# Retuning a Live Chain

Settings consumed by the mining controller and the spammer can be changed **without restarting the whole stack**. The control plane applies mining settings inside the resident worker at a scheduler safe point; the transitional spam path replaces only the spammer container. The nodes keep running and the chain is preserved.

Three equivalent paths perform the same operation:

- **Manual** (below): edit `.env`, recreate the affected service(s).
- **Control-plane UI**: `docker compose --profile control-plane up -d`, then
  [http://localhost:8090/](http://localhost:8090/) — edit, Apply. The control plane validates
  first, stores durable desired state, mirrors the transitional `.env` settings (managed keys are canonicalized into one
  `# Managed by simchain panel` block; your other lines are preserved), recreates only
  the spammer when needed, and rolls back automatically if any component rejects the apply.
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

The manual fallback is safe mid-run because `--force-recreate` replaces only the named services; the node dependencies remain running, so the chain, wallets, and mempool survive. The control-plane path is safer for mining because it does not replace the worker and can report desired/effective generations and its exact safe-point phase.

## Caveats

- Settings consumed by the **nodes** (`BTC_IMAGE`, host ports, `MIN_RELAY_TX_FEE`,
  ZMQ ports, ...) do require recreating the nodes, and node containers keep the chain
  in their filesystem, so that resets the chain: use a full
  `docker compose --profile "*" down`, then bring up the profile you want.
- `FALLBACK_FEE` is shared: the spammer prices its floor fills with it, and the nodes
  take it as `-fallbackfee` (wallet-side fallback). A spammer-only recreate moves the
  spam fee floor immediately; the nodes keep the old wallet fallback until a full
  restart, which is usually irrelevant.
