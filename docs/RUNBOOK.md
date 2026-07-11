# Runbook

Handy `bitcoin-cli` one-liners against the simnet. This how all this started... trying a bunch of docker commands

> The credentials in the commands below (`-rpcuser=foo -rpcpassword=rpcpassword`)
> are the defaults; replace them with your `BTC_RPC_USER` / `BTC_RPC_PASS` from
> `.env`.

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

## P2P latency and packet loss

Apply delay and optional loss to a node's P2P interface without affecting its RPC or
helper traffic:

```bash
./scripts/netem.sh apply btc-simnet-node3 --delay-ms 500 --loss-pct 1
./scripts/netem.sh status btc-simnet-node3
./scripts/netem.sh clear btc-simnet-node3
```

The first command builds the small `docker/netem.Dockerfile` helper if needed. The helper
is one-shot; only it receives `NET_ADMIN`. The qdisc lives in the target node's network
namespace and therefore disappears when that node is restarted or recreated.

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
