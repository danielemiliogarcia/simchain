# Implementation plan: network partition / latency simulation

## Status: READY TO IMPLEMENT (written 2026-07-10)

Implements nice-to-have **"2. Network partition / latency simulation"** from
[NICE-TO-HAVE.md](NICE-TO-HAVE.md). That entry explains the user-facing motivation; this
document is the engineering hand-off: exact scope, architecture, file-level changes, and
the verification plan.

## 1. Goal and non-goals

**Goal:** add post-bootstrap tooling that can:

- isolate one miner node from the Bitcoin P2P mesh while keeping RPC/control access to
  every node,
- mine both sides of the split explicitly so competing branches grow organically,
- heal the split and wait for the longer branch to win network-wide,
- optionally inject latency / packet loss on **P2P traffic only**, without impairing RPC,
  health checks, or helper-container control traffic.

User-facing phase-1 commands:

```bash
./scripts/partition.sh run btc-simnet-node3 --main-blocks 3 --isolated-blocks 4
./scripts/partition.sh disconnect btc-simnet-node3
./scripts/partition.sh heal btc-simnet-node3
./scripts/partition.sh status
```

User-facing phase-2 commands:

```bash
./scripts/netem.sh apply btc-simnet-node3 --delay-ms 500 --loss-pct 1
./scripts/netem.sh clear btc-simnet-node3
./scripts/netem.sh status btc-simnet-node3
```

**Non-goals:**

- No support in v1 for mutating network topology **before bootstrap completes**. The
  bootstrap funding sequence is height-coded and assumes a connected miner pair; splitting
  the miners before height 204 risks skipping intended funding stages.
- No arbitrary peer graph editor or per-peer impairment matrix. v1 supports "isolate one
  miner from the rest" and per-node netem, not a general network lab.
- No host-to-node or RPC latency simulation. The point is chain/tx propagation between
  bitcoind peers; helper control traffic must stay clean.
- No attempt to keep the current mining controller running through a split. Its sync logic
  assumes a single chain and would stall across diverging branches.
- No permanent daemon for partitions. The main surface is explicit scripts; those same
  scripts are what the later scenario engine should reuse.

## 2. Current state (what the plan builds on)

- **All containers currently share one Docker network.** `docker-compose.yml` has a single
  `btc-simnet-network`. That means P2P, RPC, helper-to-node control traffic, and optional
  explorer traffic all share the same network attachment.
- **The miner controller cannot survive a split.**
  `crates/mining-controller/src/mining.rs` mines on one node, then waits for the other to
  reach the same height before continuing. Across a real partition that wait never
  finishes, so leaving the controller running would deadlock the simulation.
- **The reorg simulator is administrative, not organic.** It forces a winner with
  `invalidateblock`; that is useful, but it does not model propagation-delay-caused
  competing chains.
- **Node3 is intentionally not host-exposed.** A host-side helper cannot reach it directly
  today except through the Docker network. Any feature that disconnects node3 from the
  only network would also cut off helper access to its RPC.
- **The repo already has the right operational primitives.**
  `scripts/simulate-reorg.sh`, `scripts/chainwatch.sh`, and the runbook all assume the
  repo root is the control point for Docker + RPC orchestration. A partition helper fits
  naturally beside them.
- **The chain now persists on named volumes.** That makes partition experiments safe to
  stop/resume and easy to combine with snapshots.

## 3. The enabling change: split control traffic from P2P traffic

The original sketch ("`docker network disconnect/connect` or `tc netem`") is directionally
right but incomplete for this repo. With today's single-network topology, disconnecting a
node would sever **everything**:

- peer-to-peer gossip,
- helper-container RPC to that node,
- mining controller access,
- spammer access,
- any future netem would also slow RPC, not just P2P.

So the feature's first required change is architectural:

### New network model

- `btc-simnet-control`
  - RPC/control-plane network
  - nodes + all helper containers
  - stable service names used by `NODE*_RPC_URL`
- `btc-simnet-p2p`
  - node-to-node gossip network only
  - node1/node2/node3 only
  - explicit per-network aliases used by `-addnode`

This is the core decision that makes both phases workable:

- phase 1 can disconnect a node from `btc-simnet-p2p` only,
- phase 2 can apply `tc netem` to the P2P interface only,
- helper tools remain fully functional over `btc-simnet-control`.

## 4. Change 1: update `docker-compose.yml` to a two-network topology

### Top-level networks

Replace the single top-level network with:

```yaml
networks:
  btc-simnet-control:
    name: btc-simnet-control
  btc-simnet-p2p:
    name: btc-simnet-p2p
    ipam:
      config:
        - subnet: 172.30.0.0/24
```

The fixed subnet is not for users; it makes the later netem helper's P2P-interface
discovery deterministic.

### Node attachments

Each node joins both networks:

- control network under its existing service/container name,
- p2p network with explicit aliases:
  - `node1-p2p`
  - `node2-p2p`
  - `node3-p2p`

### Tool attachments

`btc-simnet-mining-controller`, `btc-simnet-spammer`, `btc-simnet-reorg`, `electrs`,
`mempool-*`, and any future helper containers join **only** `btc-simnet-control`.

### Update peer wiring

Change the nodes' `-addnode` values from the current service names to the P2P aliases:

```yaml
- -addnode=node2-p2p:18444
- -addnode=node3-p2p:18444
```

and so on for each node.

This avoids Docker DNS ambiguity once a node lives on two networks. RPC keeps using the
existing `NODE*_RPC_URL` defaults on the control network; only P2P peer discovery moves.

## 5. Change 2: add `scripts/partition.sh`

The partition helper is a host-side script, same convention as `snapshot.sh` and
`simulate-reorg.sh`: resolve `REPO_ROOT`, use `docker compose -f ... --project-directory
...`, read `.env` for RPC credentials and defaults, and print operator-readable status.

### Supported commands

```text
partition.sh run <miner-node> [--main-blocks N] [--isolated-blocks M] [--keep-spammer]
partition.sh disconnect <miner-node>
partition.sh heal <miner-node>
partition.sh status
```

`<miner-node>` is limited to:

- `btc-simnet-node2`
- `btc-simnet-node3`

v1 does **not** support isolating node1. That is operationally possible, but not the main
organic-reorg use case and adds needless surface.

### `run`

The deterministic happy path:

1. Require the simnet to be running and `node1` height to be `>= 204`.
2. Record whether `btc-simnet-mining-controller` is running; stop it.
3. Stop `btc-simnet-spammer` by default too; `--keep-spammer` leaves it up.
   Reason: the spammer keys off node1's height and would keep feeding the isolated miner
   based on the main side's block arrivals, which is not the default operator expectation.
4. Disconnect the target node from `btc-simnet-p2p` only.
5. Verify:
   - target-node RPC still reachable over control network,
   - peer counts reflect the split (`getpeerinfo`),
   - main-side nodes still see each other.
6. Mine `--main-blocks` on the connected side's miner and `--isolated-blocks` on the
   isolated miner, explicitly by RPC.
7. Reconnect the isolated node to `btc-simnet-p2p` with its original alias.
8. Trigger fast reconnection by issuing `addnode ... onetry` from both sides using the
   P2P aliases; do not wait for bitcoind's background reconnect timer.
9. Poll until node1, node2, and node3 converge on the same best block hash.
10. Restart whichever services the helper stopped.

### Why block counts, not just "wait N seconds"

For v1, the main `run` surface should be block-count-driven:

- deterministic,
- CI-friendly,
- guarantees a longer winner if `main-blocks != isolated-blocks`,
- avoids timing races from variable block intervals.

Timed partitions can be added later, but the base tool should not depend on wall-clock
luck to produce the winner.

### Guard rails

- Reject `main-blocks == isolated-blocks` unless `--allow-tie` is explicitly added later.
  Equal-length branches after heal create non-deterministic winners, which is the wrong
  default for a test tool.
- Refuse to run if bootstrap is incomplete (`height < 204`).
- Refuse to run if a prior manual disconnect is still active.

### `disconnect` / `heal`

These are manual operator tools:

- `disconnect` severs the target miner from `btc-simnet-p2p`,
- `heal` reattaches it with its alias and issues the `addnode ... onetry` refresh.

They deliberately do **not** stop/start the controller or spammer automatically. They are
manual controls for advanced users who know what else they need to pause.

### `status`

Print:

- whether each miner is attached to `btc-simnet-p2p`,
- current `getpeerinfo` counts on node1/node2/node3,
- current heights and best block hashes.

That is enough to confirm "split", "healed", and "converged" states quickly.

## 6. Change 3: manual mining inside the partition helper

`partition.sh run` should mine blocks directly with `bitcoin-cli` RPC, not by restarting
the mining controller in a special mode.

Mining target selection:

- if isolating node3:
  - connected side miner: node2
  - isolated side miner: node3
- if isolating node2:
  - connected side miner: node3
  - isolated side miner: node2

Reward address:

- default to a fresh address from the miner node's configured wallet (`NODE2_WALLET_NAME`
  / `NODE3_WALLET_NAME`),
- allow an explicit address override later if needed, but do not make it part of the
  initial CLI surface.

This keeps funds on the miner wallets and stays aligned with the existing operational
model.

## 7. Change 4: add phase-2 `tc netem` support with one-shot helper services

Latency/loss support should be implemented as one-shot helper services, not by modifying
the Bitcoin images.

### Why a helper service

- the official `bitcoin/bitcoin` image does not carry `tc`,
- adding `NET_ADMIN` to the node services themselves is broader than needed,
- a helper can share the node's network namespace and apply qdisc state without changing
  the bitcoind image.

### New image

Add:

```text
docker/netem.Dockerfile
```

Base: small Debian image with:

- `iproute2`
- `bash`
- `getent` / libc NSS tooling

### Compose services

Add profile `partition` services such as:

- `btc-simnet-netem-node1`
- `btc-simnet-netem-node2`
- `btc-simnet-netem-node3`

with:

- `network_mode: "service:btc-simnet-nodeX"`
- `cap_add: ["NET_ADMIN"]`
- shared script entrypoint from the repo mount or copied into the image

They are not long-running daemons. The host script runs them as needed:

```bash
docker compose --profile partition run --rm btc-simnet-netem-node3 apply ...
```

### `scripts/netem.sh`

Supported commands:

```text
netem.sh apply <node> --delay-ms N [--loss-pct P]
netem.sh clear <node>
netem.sh status <node>
```

The helper inside the shared namespace should:

1. resolve one P2P peer alias (`node1-p2p`, etc.) to an IP,
2. run `ip route get <peer-ip>` to determine the P2P interface,
3. apply or clear `tc qdisc` on that interface only.

That is why the fixed P2P subnet and aliases matter.

## 8. Why not use `disconnectnode` alone

Bitcoin Core RPC has peer-disconnect primitives, but they are the wrong base here.

Problems:

- they manipulate the current peer set, not the transport path itself,
- they do not help with latency/loss injection,
- persistent `-addnode` peers may reconnect unless the script keeps chasing them,
- they leave too much of the "real network fault" semantics inside bitcoind rather than
  in the transport layer.

The feature is specifically about simulating network faults. Docker-network split plus
netem on the P2P interface is the correct layer.

## 9. What needs no behavior changes

- **Mining controller code:** no changes are required to the controller itself for v1.
  The helper script simply stops it before the split and restarts it after heal.
- **Spammer code:** no changes needed for the default path; the helper script can stop and
  resume it just like the controller.
- **Reorg simulator:** unchanged. It remains useful for administrative reorgs; partition
  testing complements it rather than replacing it.
- **Node RPC URLs and helper-tool RPC code:** unchanged. Those stay on the control network.

## 10. Documentation updates (same PR)

- **README.md**
  - update the topology diagrams to show control and P2P networks,
  - add a short "Partitions and P2P latency" section with the helper commands.
- **docs/INTRO.md**
  - describe the two-network architecture.
- **docs/RUNBOOK.md**
  - add `partition.sh` and `netem.sh` recipes.
- **docs/SETTINGS.md**
  - add any new `PARTITION_*` and `P2P_*` defaults if introduced,
  - document that the feature is post-bootstrap-only in v1.
- **docs/NICE-TO-HAVE.md**
  - remove item #2 once shipped and renumber.

## 11. Verification plan

### Manual, in order

1. **Topology sanity after the network split only**
   - `docker compose up -d`
   - confirm the stack still behaves exactly as before with no partition tooling used,
   - confirm nodes peer over the P2P aliases and tools still reach them over control RPC.

2. **Deterministic partition run**
   - let the chain pass bootstrap,
   - run:

     ```bash
     ./scripts/partition.sh run btc-simnet-node3 --main-blocks 3 --isolated-blocks 4
     ```

   - confirm:
     - controller was stopped and restarted,
     - node3 was disconnected only from P2P,
     - node2 and node3 mined different tips during the split,
     - after heal, the 4-block side won and every node converged.

3. **Observed organic reorg**
   - run `./scripts/chainwatch.sh` on node1 during the same partition test,
   - confirm it reports the natural reorg when the longer branch wins after heal.

4. **Manual disconnect / heal**
   - `./scripts/partition.sh disconnect btc-simnet-node3`
   - inspect `getpeerinfo`,
   - `./scripts/partition.sh heal btc-simnet-node3`
   - confirm peer counts recover.

5. **Netem apply / clear**
   - apply `500ms` delay + `1%` loss to node3,
   - inspect `tc qdisc` state,
   - compare block / tx propagation lag across nodes,
   - clear and confirm the qdisc is gone.

6. **Coexistence with optional tool profiles**
   - run with `--profile mempool`,
   - confirm partitioning P2P does not sever `mempool-*` / `electrs` control-plane access
     to node1.

### Explicit failure-path checks

1. attempt `partition.sh run` before height 204 -> clean refusal.
2. attempt equal block counts -> clean refusal.
3. attempt `heal` when not disconnected -> clean no-op / clear message.
4. apply netem, restart the target node, verify `netem.sh status` reports cleared state
   (qdisc is namespace-local and restart resets it).

## 12. Risks and edge cases

- **This feature is impossible to do cleanly on the current single-network topology.**
  The control-vs-P2P split is not optional; it is the enabling change.
- **Docker network reconnect loses aliases unless the script re-adds them.** The helper
  must reconnect with the original P2P alias every time.
- **The mining controller will hang if left running across a split.** Stopping it is the
  correct behavior, not a workaround.
- **Keeping the spammer alive across partitions is surprising by default.** It can still
  send to the isolated miner over control RPC, keyed off node1's block arrivals. That is
  why the helper stops it unless explicitly told otherwise.
- **`tc` state is ephemeral.** Restarting a node clears qdisc state. That is acceptable;
  the helper scripts should report it, not try to persist it.

## 13. Effort and change list

Phase 1 is medium because the network split is real architecture work even though the
user-facing control is "just a script". Phase 2 is medium on top.

| File | Change |
| --- | --- |
| `docker-compose.yml` | Split `control` vs `p2p` networks, add P2P aliases, add optional `partition` profile helper services |
| `scripts/partition.sh` | New partition control script (`run`, `disconnect`, `heal`, `status`) |
| `scripts/netem.sh` | New latency/loss helper script (`apply`, `clear`, `status`) |
| `docker/netem.Dockerfile` | New tiny helper image with `iproute2` |
| `README.md` | Document the two-network topology and partition/netem usage |
| `docs/INTRO.md` | Explain control-vs-P2P network split |
| `docs/RUNBOOK.md` | Add partition/netem operational recipes |
| `docs/SETTINGS.md` | Document any new partition/netem knobs |
| `docs/NICE-TO-HAVE.md` | Remove item #2 once implemented |

## 14. Recommended implementation order

1. Split the compose topology into control and P2P networks.
2. Verify the simnet behaves identically with no partitioning applied.
3. Add `scripts/partition.sh` and land deterministic split/heal runs.
4. Add the `netem` helper image + `scripts/netem.sh`.
5. Update docs and run the full manual verification sequence.

That order keeps the highest-risk change first: the network split that makes every later
step possible.
