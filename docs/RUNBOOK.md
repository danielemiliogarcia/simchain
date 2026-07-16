# Runbook

Handy `bitcoin-cli` one-liners against the simnet. This how all this started... trying a bunch of docker commands

> The credentials in the commands below (`-rpcuser=foo -rpcpassword=rpcpassword`)
> are the defaults; replace them with your `BTC_RPC_USER` / `BTC_RPC_PASS` from
> `.env`.

## Declarative scenarios

Start the simnet plus its single control plane, then upload a scenario. The server waits
for bootstrap height 204 before executing any declared steps:

```bash
docker compose up -d --build
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

The convenience wrappers submit the same durable jobs:

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

## Bounded manual mining

Mine through the same leased job path as the dashboard, API, and MCP:

```bash
cargo run -p simchainctl -- mine --node node3 --blocks 1 --wait
```

## Bounded spam burst

Submit wallet transactions through a server-side action job:

```bash
cargo run -p simchainctl -- spam burst \
  --node node3 --txs 10 --outputs-per-tx 0 --wait
```

## Miner-prioritized zero-fee faucet

Fund one or more externally controlled regtest addresses through the control plane:

```bash
cargo run -p simchainctl -- faucet \
  --to bcrt1q...=1btc \
  --to bcrt1p...=25000000sat \
  --source auto --wait
```

Amounts require an exact `btc` or `sat` suffix. The command prints its generated
idempotency UUID before submission; reuse it with `--idempotency-key` if the client
loses the response. `--wait` stops when the durable job has armed the same signed tx on
both miners. It does not wait indefinitely for mining.

Inspect treasury availability and follow delivery separately:

```bash
cargo run -p simchainctl -- faucet status
cargo run -p simchainctl -- faucet transfer TXID --watch --timeout 900
```

The transaction spends real regtest miner funds and pays exactly 0 sat in actual fees.
The displayed 100 BTC delta is virtual miner-local priority only; it is not paid or
transferred. While delivery is armed, another faucet, reorg, scenario, partition, or
degradation is rejected. Spam remains live, read operations remain available, and a
bounded manual mine is allowed.

If mining was manually paused before the request, the faucet restores that paused
state after arming. Mine the first block through the coordinated action, then watch the
transfer become confirmed:

```bash
cargo run -p simchainctl -- mine --node node3 --blocks 1 --wait
cargo run -p simchainctl -- faucet transfer TXID --watch
```

For `insufficient_faucet_funds`, inspect `faucet status`. Reduce the request, choose the
other source treasury, or lower `FAUCET_WALLET_RESERVE_BTC` and restart the control
plane only if the test's reserve policy permits it. Do not refill by weakening relay or
mempool policy.

For `faucet_delivery_pending`, inspect the pending txid and both miner mempools, then
mine a block or let normal mining continue. The control-plane delivery guard repairs a
missing miner copy from its private durable transaction before manual mining. On a
control-plane restart it resumes the same prepared txid; it never constructs a second
payment. The private recovery record is in the `btc-simnet-control-state` named volume
and must not be copied into logs or public API output.

An abort before submission unlocks inputs and clears any priority entries. An abort
after either miner accepted the tx cannot retract it: the job reports the txid as
`aborted_after_submission`, clears owned virtual priority where possible, and the tx
may still confirm. Always inspect that transfer before issuing a replacement payment.

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

Run a durable three-block reorg and wait for convergence and cleanup:

```bash
cargo run -p simchainctl -- reorg --depth 3 --wait
```

Chaos reorg: replace them with empty blocks so orphaned transactions stay unconfirmed:

```bash
cargo run -p simchainctl -- reorg --depth 3 --empty --wait
```

The standalone direct-RPC profile remains available for continuous low-level testing:

```bash
REORG_MODE=auto docker compose --profile reorg up btc-simnet-reorg
```
