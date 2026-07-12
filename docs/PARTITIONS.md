# Network Partitions and P2P Latency

Real reorgs are *caused* by propagation delays and network partitions; the reorg
simulator ([REORGS.md](REORGS.md)) forces one administratively with `invalidateblock`.
The partition tooling reproduces the real mechanism instead: it isolates one miner from
the Bitcoin P2P mesh while keeping RPC control access to every node, lets both sides
mine competing branches blind to each other, then heals the split so the heavier branch
wins network-wide — an **organic reorg**, with natural orphan races and double-spend
windows. Separate helpers make a node's P2P link slow or lossy, making block/tx
propagation observable in an otherwise instantaneous regtest network.

All tools rely on the two-network topology: every container shares
`btc-simnet-control` (RPC, healthchecks, helper tools), while only the three bitcoind
nodes join `btc-simnet-p2p` (gossip, via the `node1-p2p`/`node2-p2p`/`node3-p2p`
aliases). Cutting or impairing the P2P attachment therefore never affects RPC access,
the mining controller, the spammer, or the explorer stack.

Three scripts, two layers:

| Script | What it does |
| --- | --- |
| `partition.sh` | split one miner off the P2P mesh, heal it back, inspect the state |
| `degrade.sh` | make one node slower and/or lossy for N seconds or N blocks, then auto-restore (simple layer) |
| `netem.sh` | raw apply/clear/status of P2P latency and loss, no time limit (advanced layer under `degrade.sh`) |

## Splitting the network: `partition.sh`

```text
partition.sh run <miner-node> [--main-blocks N] [--isolated-blocks M] [--keep-spammer]
partition.sh disconnect <miner-node>
partition.sh heal <miner-node>
partition.sh status
```

- `<miner-node>` — the node to isolate: `btc-simnet-node2` or `btc-simnet-node3`.
  node1 (the user endpoint) cannot be isolated.
- `--main-blocks N` — blocks mined on the connected side during `run`
  (default 3, or `PARTITION_MAIN_BLOCKS`).
- `--isolated-blocks M` — blocks mined by the isolated miner during `run`
  (default 4, or `PARTITION_ISOLATED_BLOCKS`). Must differ from `--main-blocks`;
  the larger side is the winner.
- `--keep-spammer` — leave the spammer running during `run` instead of pausing it.

### Deterministic partition run

Post-bootstrap only (node1 at height 204 or higher). One command produces a complete
organic reorg:

```bash
./scripts/partition.sh run btc-simnet-node3 --main-blocks 3 --isolated-blocks 4
```

It pauses the mining controller (and the spammer, unless `--keep-spammer` is passed),
waits for all three nodes to share one tip, detaches node3 from the P2P network only,
verifies the split, mines 3 blocks on the connected side (node2) and 4 on isolated
node3, reattaches node3, verifies that every node converged specifically on the
longer branch it mined, and restarts the services it stopped. A failed run heals the
split and restores the stopped services on its way out. Defaults and timeouts
(`PARTITION_*`) are in [SETTINGS.md](SETTINGS.md).

With the default counts the **isolated** side wins: 4 > 3, so on heal the connected
side's blocks are orphaned and the network reorgs onto the previously-isolated miner's
chain. "Main" means "still connected to node1", not "the side that wins" — flip the
counts to make the connected side win instead.

### Manual disconnect / heal

For full control — pick what to pause, mine or inject transactions on each side, decide
when to heal. Manual commands manage no services:

```bash
docker compose stop btc-simnet-mining-controller btc-simnet-spammer
./scripts/partition.sh disconnect btc-simnet-node3
./scripts/partition.sh status        # node3 detached, 0 peers, tips diverge as you mine
# mine or submit transactions on each side as needed
./scripts/partition.sh heal btc-simnet-node3
./scripts/partition.sh status        # all nodes converged on one tip
docker compose start btc-simnet-mining-controller btc-simnet-spammer
```

`status` prints each node's P2P attachment, peer count, height and best block hash —
enough to confirm "split", "healed" and "converged" at a glance.

## Watching it live

Three terminals give a live view of both branches with the tools already in the repo:

```bash
# terminal 1: live table — P2P attachment, peers, height, tip per node
watch -n 2 ./scripts/partition.sh status

# terminal 2: reorg reporter — fork point, replaced range, new tip
./scripts/chainwatch.sh

# terminal 3: drive the partition
./scripts/partition.sh run btc-simnet-node3 --main-blocks 3 --isolated-blocks 4
```

During the split the isolated node's row shows `detached`, 0 peers, and a height
climbing on its own tip while the other rows advance on the main tip. At heal the
hashes snap to one value and chainwatch prints the reorg.

The mempool.space explorer (`mempool` / `all-tools` profiles, `localhost:1080`) follows
node1, which always stays on the main side: it shows the main branch during the split
and, after heal, marks the orphaned main-side blocks as reorged. It never shows the
isolated branch while it grows — watching both sides graphically would need a second
viewer pointed at the isolated node.

## Degrading a node: `degrade.sh`

The simple layer: make one node slower and/or lossy for a bounded window, observe, and
get the link restored automatically — one command, nothing to remember to undo.

```text
degrade.sh <node> <delay-ms> <loss-pct> <duration>
```

- `<node>` — any of the three nodes (`btc-simnet-node1|2|3`).
- `<delay-ms>` — extra one-way delay on packets the node sends, in milliseconds
  (`0` = none).
- `<loss-pct>` — percent of sent packets dropped, `0`–`100` (`0` = none).
- `<duration>` — how long to hold the degraded link: `30s` = 30 seconds,
  `5b` = until 5 new blocks are mined network-wide (a bare number means seconds).
  Block mode needs the mining controller (or manual mining) running.

```bash
# node3 sends everything 500ms late and drops 1% of packets, for 60 seconds
./scripts/degrade.sh btc-simnet-node3 500 1 60s

# node3 is 2 seconds slow until 5 blocks have been mined
./scripts/degrade.sh btc-simnet-node3 2000 0 5b
```

Inside it is just `netem.sh apply` → hold the window → `netem.sh clear`, with the
restore guaranteed on exit (Ctrl+C restores early). While it holds, watch
`./scripts/partition.sh status` in another terminal: the degraded node's height lags
right after each new block, then catches up.

Choosing values: what matters is the ratio between delay and block interval. At the
default 15s blocks, 500ms is clearly visible (~3% of the interval — proportionally like
20-second propagation on mainnet). If you retune the simnet toward mainnet-like 10-minute
blocks, the same 500ms becomes negligible — which is faithful: that is exactly why
mainnet rarely reorgs. For visible latency effects, scale the delay with the block
interval; for reorg events at long intervals, use `partition.sh` instead.

## Fine control: `netem.sh`

The advanced layer under `degrade.sh`: apply and clear are separate steps with no time
limit, so the impairment stays until you remove it.

```text
netem.sh apply <node> --delay-ms N [--loss-pct P]
netem.sh clear <node>
netem.sh status <node>
```

- `<node>` — any of the three nodes.
- `--delay-ms N` — one-way egress delay in milliseconds (required; `0` allowed when
  only loss is wanted).
- `--loss-pct P` — percent of sent packets dropped (optional, default `0`).

```bash
./scripts/netem.sh apply btc-simnet-node3 --delay-ms 500 --loss-pct 1
./scripts/netem.sh status btc-simnet-node3
./scripts/netem.sh clear btc-simnet-node3
```

The first command builds the small `docker/netem.Dockerfile` helper image if needed.
The helper is one-shot: it enters the node's network namespace, finds the interface
routed to the fixed P2P subnet (`172.30.0.0/24`), applies or clears the `tc netem`
qdisc, and exits. Only the helper receives `NET_ADMIN`; the bitcoind images are
unchanged.

To see the effect, compare heights across nodes right after a new block: the impaired
node lags. Useful against zero-conf assumptions, ZMQ consumers, and indexers that never
saw a slow network on regtest.

## What it simulates

- **Organic reorgs** — two chains grow independently, the heavier one wins on
  reconnect: the real-world mechanism, rather than an administrative rewrite.
- **Double-spend windows** — a transaction confirmed on the isolated branch loses its
  confirmations when the other side wins. The scenario exchanges, custody watchers and
  payment processors must detect; test confirmation-depth handling against it.
- **Orphan races** — competing blocks at the same height with a natural winner.
- **Propagation lag** — netem makes gossip latency observable and measurable across
  nodes.

## Design notes

Rationale worth keeping now that the implementation plan is retired.

### Why two networks

This feature is impossible to do cleanly on a single Docker network. With everything on
one network, disconnecting a node severs P2P gossip *and* helper RPC, mining-controller
and spammer access; netem would slow RPC alongside P2P. Splitting control from P2P is
the enabling change: partitions detach a node from `btc-simnet-p2p` only, netem impairs
only the P2P interface, and every tool keeps working over `btc-simnet-control`. The
P2P network's fixed subnet (`172.30.0.0/24`) is not for users — it makes the netem
helper's interface discovery deterministic (resolve a peer's P2P alias, `ip route get`,
apply `tc` to that interface only).

### Why Docker-level disconnect, not `disconnectnode` RPC

Bitcoin Core has peer-disconnect RPCs, but they manipulate the peer set, not the
transport path: persistent `-addnode` peers reconnect unless a script keeps chasing
them, and they offer nothing for latency/loss injection. The feature simulates network
faults, so the fault belongs in the transport layer — Docker network detach for
partitions, netem qdisc for impairment. (`partition.sh` still issues `disconnectnode`
once after detaching, only to flush TCP sessions that Core would otherwise consider
alive until their next failed write; and `addnode ... onetry` after healing, to avoid
waiting on bitcoind's background reconnect timer.)

### Why block counts, not seconds

`run` takes `--main-blocks`/`--isolated-blocks` instead of a duration: deterministic,
CI-friendly, and a guaranteed longer winner when the counts differ. A timed partition
depends on wall-clock luck against variable block intervals. `run` also refuses when
any node is already detached from a prior manual `disconnect`.

### Why the controller and spammer are paused

The mining controller mines on one node and then waits for the other miner to reach the
same height — across a partition that wait never finishes, so leaving it running would
deadlock. The spammer keys off node1's block arrivals and can still reach the isolated
miner over control RPC, feeding it based on the *other* branch's cadence — surprising
as a default, hence stopped unless `--keep-spammer`. Neither tool needed code changes;
the script stops and restarts them around the split.

### Why netem runs as a one-shot helper

The official `bitcoin/bitcoin` image carries no `tc`, and granting `NET_ADMIN` to the
node services themselves is broader than needed. The helper image (Debian +
`iproute2`) joins the node's network namespace (`network_mode: "service:..."`), applies
or clears the qdisc, and exits — only the short-lived helper ever holds `NET_ADMIN`,
and the bitcoind images stay stock. `degrade.sh` composes `netem.sh` rather than
reimplementing it, so both layers share one code path to the kernel.

### Mining rewards

During `run`, each side mines to a fresh address from that miner node's configured
wallet (`NODE2_WALLET_NAME` / `NODE3_WALLET_NAME`), keeping funds on the miner wallets
like the normal mining path does.

## Caveats

The netem caveats apply equally to `degrade.sh` — it is netem underneath.

- **Egress-only netem.** A root netem qdisc shapes packets the node *sends*, not what
  it receives: a 500ms delay adds 500ms one way (RTT +500ms, not +1000ms). Apply it
  on both endpoints for symmetric latency.
- **Ephemeral qdisc.** The qdisc lives in the node's network namespace and disappears
  when the node is restarted or recreated; `netem.sh status` reports the live state.
- **Docker path only — separation is convention, not enforcement.** Partitions and
  netem act on the Docker P2P network path. The nodes have no `-bind`, so bitcoind
  listens on every interface: a partitioned node still *accepts* P2P connections over
  the control network (nothing in the stack dials one there), and host-side P2P through
  the published ports (e.g. `localhost:18444` into node1) bypasses both tools — keep
  external nodes disconnected during experiments. Future hardening: static per-node
  P2P IPs plus `-bind=<p2p-ip>:18444`.
- **Post-bootstrap only.** The bootstrap funding sequence assumes a connected miner
  pair; `run` refuses below height 204.

Operational one-liners: [RUNBOOK.md](RUNBOOK.md). Settings and defaults:
[SETTINGS.md](SETTINGS.md).
