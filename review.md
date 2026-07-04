# Simchain Code Review Report

Date: 2026-07-04 · Scope: full repository at `master` (ff246a0)

## Project understanding

Simchain spins up a private Bitcoin **regtest** network that behaves as close to mainnet as
regtest allows, so that other projects can be tested against it and later moved to
testnet/mainnet with minimal changes:

- **node1**, user-facing "production-like" full node (RPC exposed to the host, txindex,
  not a miner).
- **node2**, user-owned node with wallet enabled (RPC exposed on 28443). Miner.
- **node3**, internal-only peer, not reachable from the host. Miner.
- **mining-controller** (Rust), funds the user address with two coinbase outputs (2×50 BTC),
  matures them (102 blocks), then mines one block every `BLOCK_INTERVAL_SECS`, alternating
  between node2 and node3.
- **spammer** (Rust), after each new block, sends `SPAM_PER_MINER_PER_BLOCK` dust
  transactions from node2→node3 and node3→node2 wallets so blocks are never empty.
- Optional explorer stack (`mempool/frontend`, `mempool/backend`, `mariadb`, `electrs`) in a
  second compose file.

The design is sound for its purpose. The issues below are ordered by severity within each
area.

---

## Findings

### Orchestration (docker-compose)

1. **Startup race conditions, no readiness gating.** `depends_on` (short syntax) only
   orders container *start*, not bitcoind *readiness*. The mining controller sleeps 200 ms
   and then calls the RPC with `.unwrap()`; if a node is slow to boot (first pull, slow
   disk) the controller panics and, since no `restart` policy is set, the network comes up
   unfunded and never mines. Fix: node healthchecks (`bitcoin-cli getblockcount`) plus
   `depends_on: condition: service_healthy`. *(Fixed in this change set.)*

2. **Two compose files with an ordering trap.** `docker-compose-mempool.yml` declares the
   network as `external: true`, so it hard-fails unless the main stack was started first,
   and both files duplicate service/network config. Compose *profiles* solve this cleanly
   in a single file. *(Fixed in this change set, profiles `electrs`, `mempool`, `all-tools`.)*

3. **README/compose contradiction on node1's wallet.** README says node1 simulates a
   production endpoint with `-disablewallet=1`, but the flag is commented out in
   `docker-compose.yml`, so node1 actually has a wallet. Either behavior is defensible;
   docs and config should agree. *(Fixed: env-configurable via `NODE1_DISABLE_WALLET`,
   defaulting to `1`, the original design intent: no hot wallet on the user endpoint.)*

4. **Hard-coded values everywhere.** Image tags, ports, fee policy, mempool/electrs
   versions, DB credentials were all inline; only RPC creds and 4 app settings came from
   `.env`. *(Fixed, everything is now `${VAR:-default}`.)*

5. **RPC exposed on all host interfaces.** `-rpcallowip=0.0.0.0/0` together with
   `"18443:18443"` publishes the RPC on every host interface. Fine on a laptop, risky on a
   shared/dev server, anyone who can reach the host can mine/spend on your simnet.
   Consider `127.0.0.1:18443:18443` port bindings if the host is not private.
   *(Accepted as-is: this is a dev tool and reaching the simnet from another machine
   is a wanted use case, e.g. running the simnet on a separate box.)*

6. **`version:` key is obsolete** in Compose v2 and triggers a warning. *(Removed.)*

7. **`-maxtxfee=10000000`** is denominated in BTC, ten million BTC as a cap is effectively
   "no cap". Intentional for a spam-heavy simnet, but worth a comment; it also masks fee
   bugs in code under test that a realistic cap would catch. *(Now configurable via
   `MAX_TX_FEE`.)*

8. **No restart policy on nodes.** If bitcoind OOMs or crashes mid-simulation, the network
   silently degrades. `restart: unless-stopped` on the three nodes is a reasonable default
   for a long-running simnet. *(Fixed.)*

### mining-controller (Rust)

9. **Nothing is retried; every RPC error is a panic.** `create_client(...).unwrap()`,
   `generate_to_address(...).unwrap()`, etc. A single transient error (node restarting,
   RPC warm-up) kills the process permanently. A small retry helper around the RPC calls
   would make the controller resilient; the compose healthcheck now mitigates the startup
   case but not mid-run hiccups.

10. **Not idempotent across restarts.** `setup_wallet` calls `create_wallet` and unwraps;
    if the container restarts after the wallet exists, it panics forever. It should try
    `load_wallet`/`listwallets` first and fall back to `create_wallet` (or ignore the
    "already exists" error). This is also why adding `restart: on-failure` alone would
    just produce a crash loop.

11. **Sleep-based synchronization.** Fixed sleeps of 100–1000 ms are used to "wait for
    network sync" between mining bursts. Polling `getblockcount` on the *other* nodes until
    they reach the expected height is deterministic and removes the race where competing
    blocks "stack" (the code comment itself acknowledges the race).

12. **`get_new_address(None, None)` without a wallet-scoped client.** Works only while
    exactly one wallet is loaded on the node. If a user loads a second wallet on node2
    (a documented use case, "stack an ordinals wallet on top"), the generic RPC path
    returns "wallet file not specified" and the controller dies. Use
    `http://node:18443/wallet/<name>` URLs after wallet creation. *(Fixed: controller and
    spammer use wallet-scoped clients; wallet names are configurable via
    `NODE2_WALLET_NAME`/`NODE3_WALLET_NAME`.)*

13. Minor: `env::var("BTC_RPC_USER")...parse::<String>()`, parsing a `String` into a
    `String` is a no-op; drop the `.parse()`. *(Fixed.)*

### spammer (Rust)

14. **Send errors are silently discarded.** `let _ = from.send_to_address(...)`, when the
    wallet runs out of confirmed funds or the mempool rejects a tx, the spammer reports
    nothing and blocks quietly go empty, which defeats its purpose. At minimum count and
    log failures per batch. *(Fixed: the spammer counts successes and logs
    "only X/Y accepted" with the first error.)*

15. **Wallet race with the controller.** The spammer calls `get_new_address` on node2/node3
    wallets that the *controller* creates. It gates on block height ≥ 102, which currently
    implies the wallets exist, an implicit cross-service contract that breaks if funding
    logic changes. Same multi-wallet caveat as #12 applies.

16. **`ENABLE_SPAM=false` still busy-polls forever** (a wake-up every 200 ms doing an RPC
    call). If spam is disabled the process could exit (with `restart: "no"`) or sleep long.
    *(Fixed: it exits immediately.)*

17. Minor: 540 sat outputs are below the 546-sat P2PKH dust bound but valid for bech32
    (294 sat floor), fine, but a one-line comment would prevent a future "fix".
    *(Fixed: 546 sats with an explanatory comment.)*

### Dockerfiles / build

18. **Bash `$UID`/`$GID` in `docker-entrypoint.sh` cannot come from the environment.**
    Bash pre-sets `UID`/`GID` as readonly shell variables at startup, so values passed with
    `docker run -e UID=...` are shadowed (and for root, `UID` is always 0, so the
    `usermod` branch never runs). The build-`ARG` path works; the runtime-env path is dead
    code. Rename to `PUID`/`PGID` if runtime override is wanted. *(Fixed: renamed to
    `PUID`/`PGID`.)*

19. **No `.dockerignore` for the Rust contexts.** `COPY . .` ships the host's `target/`
    directory (often hundreds of MB) into the build context on every rebuild.
    *(Fixed, `.dockerignore` added to both Rust services.)*

20. **Rust images are ~1.5 GB**, full `rust:latest` kept as the runtime image. The
    Dockerfile's own TODO is right: multi-stage build, copy the binary into
    `debian:bookworm-slim`. Cheap win.

21. **Binary authenticity is not verified.** SHA256SUMS is checked, but it is downloaded
    from the same server as the tarball, and the GPG verification block is commented out:
    integrity yes, authenticity no. Known TODO; worth prioritizing if this image is ever
    reused outside regtest.

22. Minor: `RUN echo "UID: ${UID}"` debug layers can go; several `RUN` steps could be
    merged to reduce layers.

### Configuration / docs

23. **`.env.example` ships a broken placeholder.** `USER_ADDRESS=bcrt...tf3rr` is not a
    valid address; a copy-paste of the example crashes the controller with
    "Invalid Bitcoin address". Ship a valid regtest address as the default.
    *(Fixed.)*

24. **Docs drift.** README still says the Dockerfile is "tied to `x86_64-linux-gnu`", but
    the Dockerfile has runtime arch detection; `runbook.txt` uses `bituser`/`bitpass` while
    `.env.example` says `foo`/`rpcpassword`; README uses legacy `docker-compose` syntax.
    *(Fixed in the docs update.)*

25. **Plaintext RPC credentials on the bitcoind command line** are visible via
    `docker inspect`/`ps`. `-rpcauth` (salted hash) is the upstream-recommended mechanism.
    Acceptable for regtest; do not copy this pattern to anything public.

---

## What was addressed

First pass: items 1, 2, 3 (made configurable), 4, 6, 7 (configurable), 19, 23, 24, plus
the new features requested (registry/local image switch, single compose file with
profiles, reorg simulator).

Second pass: items 11, 12, 13, 14, 16, 17, 18 fixed; item 5 accepted as intentional for
a dev tool; item 25 documented in SETTINGS.md; items 20, 21, 22 recorded as future
enhancements in the README. Item 3 resolved: `NODE1_DISABLE_WALLET` now defaults to `1`,
matching the original design intent and the README.

Still open: items 9, 10 (RPC retries and wallet-creation idempotency in the controller),
15 (spammer/controller implicit wallet contract, mitigated by the shared
`NODE*_WALLET_NAME` settings), 20, 21, 22 (tracked in README future enhancements), and 8
(nodes now have `restart: unless-stopped`, so this one is fixed too).
