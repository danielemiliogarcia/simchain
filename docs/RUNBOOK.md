# Runbook

Handy `bitcoin-cli` one-liners against the simnet. This how all this started... trying a bunch of docker commands

> The credentials in the commands below (`-rpcuser=foo -rpcpassword=rpcpassword`)
> are the defaults; replace them with your `BTC_RPC_USER` / `BTC_RPC_PASS` from
> `.env`.

## Peer management

Add a peer:

```bash
docker exec btc-simnet-node1 bitcoin-cli -regtest -rpcuser=foo -rpcpassword=rpcpassword addnode btc-simnet-node2:18444 add
```

Inspect connected peers:

```bash
docker exec btc-simnet-node1 bitcoin-cli -regtest -rpcuser=foo -rpcpassword=rpcpassword getpeerinfo
```

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
