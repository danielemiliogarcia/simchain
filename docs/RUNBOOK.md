# Runbook

Handy `bitcoin-cli` one-liners against the simnet. This how all this started... trying a bunch of docker commands

> The credentials in the commands below (`-rpcuser=foo -rpcpassword=rpcpassword`)
> are the defaults; replace them with your `BTC_RPC_USER` / `BTC_RPC_PASS` from
> `.env`.

## Declarative scenarios

Start the regular simnet first, then run a one-shot scenario. The engine waits for
bootstrap height 204 before executing any declared steps:

```bash
docker compose up -d
SCENARIO_FILE=scenarios/pause-then-burst.yml \
  docker compose --profile scenario run --rm --build btc-simnet-scenario
```

Other shipped histories:

```bash
SCENARIO_FILE=scenarios/reorg-during-sync.yml docker compose --profile scenario run --rm btc-simnet-scenario
SCENARIO_FILE=scenarios/partition-node3.yml docker compose --profile scenario run --rm btc-simnet-scenario
```

Write a machine-readable CI artifact and propagate the container's exit code with:

```bash
SCENARIO_FILE=scenarios/reorg-during-sync.yml \
SCENARIO_RESULT_FILE=/workspace/results/reorg.json \
  docker compose --profile scenario run --rm btc-simnet-scenario
```

For a one-shot burst outside a YAML run:

```bash
./scripts/spam-burst.sh btc-simnet-node2 --txs 100 --outputs-per-tx 25
```

See [SCENARIOS.md](SCENARIOS.md) for the schema and failure cleanup policy.

## Peer management

Add a peer:

```bash
docker exec btc-simnet-node1 bitcoin-cli -regtest -rpcuser=foo -rpcpassword=rpcpassword addnode node2-p2p:18444 add
```

Inspect connected peers:

```bash
docker exec btc-simnet-node1 bitcoin-cli -regtest -rpcuser=foo -rpcpassword=rpcpassword getpeerinfo
```

## Network partitions

Partition runs are post-bootstrap only (node1 must be at height 204 or higher). This
command pauses the mining controller and spammer, disconnects node3 from P2P only, mines
three blocks on the connected side and four on node3, heals, waits for convergence, and
restores the services it stopped:

```bash
./scripts/partition.sh run btc-simnet-node3 --main-blocks 3 --isolated-blocks 4
```

Note who wins with those defaults: the **isolated** miner. Its 4-block branch is longer,
so on heal the connected side's three blocks are orphaned and every node reorgs onto
node3's chain — "main" means "still connected to node1", not "the side that wins".

Use `--keep-spammer` to leave the spammer running. The block counts must differ, otherwise
the winning branch would be nondeterministic. Manual controls do not stop or restart any
services:

```bash
docker compose stop btc-simnet-mining-controller btc-simnet-spammer
./scripts/partition.sh disconnect btc-simnet-node3
./scripts/partition.sh status
# Mine or submit transactions on each side as needed.
./scripts/partition.sh heal btc-simnet-node3
docker compose start btc-simnet-mining-controller btc-simnet-spammer
```

The allowed isolated miners are `btc-simnet-node2` and `btc-simnet-node3`. A failed
`run` attempts to heal the P2P attachment and restores any services it stopped.
Before disconnecting, `run` also waits for all three nodes to share one tip; after
healing, it verifies that they converged specifically on the longer branch it mined.

Partitions cut the Docker P2P network path only. Host-side P2P connections through the
published ports (e.g. `localhost:18444` into node1) bypass the partition, so keep
external nodes disconnected during partition experiments.

## P2P latency and packet loss

Simple layer — degrade a node's P2P link for a bounded window, auto-restored
(`60s` = seconds, `5b` = until 5 blocks are mined; Ctrl+C restores early):

```bash
./scripts/degrade.sh btc-simnet-node3 500 1 60s
./scripts/degrade.sh btc-simnet-node3 2000 0 5b
```

Advanced layer — apply delay and optional loss with no time limit, remove it yourself:

```bash
./scripts/netem.sh apply btc-simnet-node3 --delay-ms 500 --loss-pct 1
./scripts/netem.sh status btc-simnet-node3
./scripts/netem.sh clear btc-simnet-node3
```

The first command builds the small `docker/netem.Dockerfile` helper if needed. The helper
is one-shot; only it receives `NET_ADMIN`. The qdisc lives in the target node's network
namespace and therefore disappears when that node is restarted or recreated.

Netem shapes egress only: it delays/drops packets the node sends, not packets it
receives. `--delay-ms 500` adds 500ms one way (RTT +500ms, not +1000ms); apply it to
both endpoints for symmetric latency. It also affects only the Docker P2P interface —
host-side P2P traffic through the published ports bypasses it.

## Manual mining

Mine manually (e.g. after stopping the mining controller):

```bash
while true; do
  docker exec btc-simnet-node3 bitcoin-cli -regtest -rpcuser=foo -rpcpassword=rpcpassword generatetoaddress 1 bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr
  sleep 5
done
```

## Manual spam

Spam manually:

```bash
while true; do
    for i in $(seq 0 10); do
      docker exec btc-simnet-node3 bitcoin-cli -regtest -rpcuser=foo -rpcpassword=rpcpassword sendtoaddress "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr" 0.0000050$i "spam$i"
    done
  sleep 5
done
```

## UTXOs & balance

Get UTXOs:

```bash
docker exec btc-simnet-node3 bitcoin-cli -regtest -rpcuser=foo -rpcpassword=rpcpassword scantxoutset start '["addr(bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr)"]'
```

Get total balance:

```bash
docker exec btc-simnet-node3 bitcoin-cli -regtest -rpcuser=foo -rpcpassword=rpcpassword scantxoutset start '["addr(bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr)"]' | jq '[.unspents[].amount] | add'
```

## Snapshots

Archive the running chain (blocks, wallets, mempool) and bring it back later,
skipping bootstrap and funding (recipes: [SNAPSHOTS.md](SNAPSHOTS.md)):

```bash
./scripts/snapshot.sh save mysnap
./scripts/snapshot.sh restore mysnap
./scripts/snapshot.sh list
```

## Reorgs

One-shot reorg of the last 3 blocks:

```bash
./scripts/simulate-reorg.sh 3
```

Chaos reorg: replace them with empty blocks (orphaned txs stay unconfirmed):

```bash
./scripts/simulate-reorg.sh 3 empty
```

Continuous reorgs: every `AUTO_REORG_EVERY_BLOCKS` blocks, reorg `REORG_DEPTH` blocks:

```bash
REORG_MODE=auto docker compose --profile reorg up btc-simnet-reorg
```
