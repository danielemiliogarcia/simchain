# Retuning a Live Chain

Settings consumed by the mining controller and the spammer can be changed **without restarting the whole stack**: the nodes keep running and the chain is preserved, only the tool containers are replaced. This is the quickest way to experiment with mining cadence, the fee floor or how full blocks are, on a chain that is already bootstrapped and funded.

Three equivalent paths perform the same operation:

- **Manual** (below): edit `.env`, recreate the affected service(s).
- **Panel UI**: `docker compose --profile panel up -d`, then
  [http://localhost:8090/](http://localhost:8090/) — edit, Apply. The panel validates
  first, rewrites `.env` (managed keys are canonicalized into one
  `# Managed by simchain panel` block; your other lines are preserved), recreates only
  the affected tool service(s), and rolls back automatically if the retuned tool fails
  to come back up.
- **Panel API / MCP**: `POST /api/v1/apply` with the `.panel-token` bearer token, or the
  `apply_settings` MCP tool at `http://localhost:8090/mcp` — same semantics, built for
  scripts and coding agents (see the README's "Dashboard / control panel" section).

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

This is safe mid-run: `--force-recreate` only replaces the services named on the command line (the node dependencies are left running, so the chain, wallets and mempool survive). The mining controller sees the chain is already bootstrapped (height >= 204), skips the funding sequence and resumes mining with the new cadence; the spammer is stateless between cycles and resumes with the new fill/fee settings.

## Caveats

- Settings consumed by the **nodes** (`BTC_IMAGE`, host ports, `MIN_RELAY_TX_FEE`,
  ZMQ ports, ...) do require recreating the nodes, and node containers keep the chain
  in their filesystem, so that resets the chain: use a full
  `docker compose --profile "*" down`, then bring up the profile you want.
- `FALLBACK_FEE` is shared: the spammer prices its floor fills with it, and the nodes
  take it as `-fallbackfee` (wallet-side fallback). A spammer-only recreate moves the
  spam fee floor immediately; the nodes keep the old wallet fallback until a full
  restart, which is usually irrelevant.
