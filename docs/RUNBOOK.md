# Runbook

Handy `bitcoin-cli` one-liners against the simnet. This how all this started... trying a bunch of docker commands

> The credentials in the commands below (`-rpcuser=foo -rpcpassword=rpcpassword`)
> are the defaults; replace them with your `BTC_RPC_USER` / `BTC_RPC_PASS` from
> `.env`.

## Declarative scenarios

Start the simnet plus its single control plane, then upload a scenario. The server waits
for bootstrap height 204 before executing any declared steps:

```bash
docker compose --profile control-plane up -d --build
cargo run -p simchainctl -- scenario run scenarios/pause-then-burst.yml
```

Other shipped histories:

```bash
cargo run -p simchainctl -- scenario run scenarios/reorg-during-sync.yml
cargo run -p simchainctl -- scenario run scenarios/partition-node3.yml
```

Write a machine-readable CI artifact and propagate the job's stable exit code with:

```bash
cargo run -p simchainctl -- scenario run scenarios/reorg-during-sync.yml \
  --result results/reorg.json
```

For an external-test barrier, start `scenarios/ci-checkpoint.yml`, wait for
`mempool_loaded`, run the downstream assertions, then release it. See
[SCENARIOS.md](SCENARIOS.md) for the complete copy-paste workflow.

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
job leases mining, spam, and node3's private network agent; mines three blocks on the
connected side and four on node3; heals; and witnesses the expected winner:

```bash
cargo run -p simchainctl -- partition \
  --node node3 --main-blocks 3 --isolated-blocks 4 --wait
```

Note who wins with those defaults: the **isolated** miner. Its 4-block branch is longer,
so on heal the connected side's three blocks are orphaned and every node reorgs onto
node3's chain — "main" means "still connected to node1", not "the side that wins".

The block counts must differ, otherwise the winning branch would be nondeterministic.
The allowed isolated miners are `node2` and `node3`. A failed or aborted job heals its
owned impairment and waits for convergence before releasing spam and mining. The old
manual detach/heal commands were removed because they had no TTL owner.

Partitions cut the Docker P2P network path only. Host-side P2P connections through the
published ports (e.g. `localhost:18444` into node1) bypass the partition, so keep
external nodes disconnected during partition experiments.

## P2P latency and packet loss

Degrade a node's P2P link for a bounded number of seconds:

```bash
cargo run -p simchainctl -- degrade \
  --node node3 --delay-ms 500 --loss-pct 1 --seconds 60 --wait
```

The compatibility wrappers submit the same durable jobs:

```bash
./scripts/partition.sh run btc-simnet-node3 --main-blocks 3 --isolated-blocks 4
./scripts/degrade.sh btc-simnet-node3 500 1 60s
```

The three resident agents each share one node namespace and receive only `NET_ADMIN`.
They have no host ports or Docker socket. Every impairment has an expiring lease; TTL
expiry clears nft/tc state if the control plane dies. Unbounded raw netem was removed.

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
