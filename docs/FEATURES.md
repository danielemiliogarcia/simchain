# Features

The [README](../README.md#features) lists these as titles only. Each one below expands
into what it actually does and points at the document that covers it in depth.

## Network and nodes

### Mainnet-like network shape

Three Bitcoin Core nodes form a full P2P mesh; two mine while a wallet-disabled,
non-mining node gives applications a production-like RPC endpoint whose native Core
whitelist rejects regtest mining superpowers and node-administration calls.

See [INTRO.md](INTRO.md) for the role of each node,
[SETTINGS.md](SETTINGS.md#node1-rpc-method-policy) for the exact allowed and denied
method lists, and [NETWORK_TOPOLOGY.md](NETWORK_TOPOLOGY.md) for how the two Docker
networks separate P2P traffic from control traffic.

### Application integration

Use Bitcoin Core RPC, all five ZMQ topics, optional Electrum RPC, and an optional local
mempool.space explorer connected to node1.

See [ZMQ notifications](../README.md#zmq-notifications) for the topic list, host ports
and a smoke test, and [SETTINGS.md](SETTINGS.md) for the electrs and mempool.space
profiles.

## Mining and fee market pressure

### Configurable, reproducible mining

Choose fixed or bounded-Poisson block intervals, strict miner alternation or weighted
selection, and an optional RNG seed for repeatable runs.

See [SETTINGS.md](SETTINGS.md#mining-controller) for every mining setting and
[RETUNING.md](RETUNING.md) for changing them on a running chain.

### Live hot-reloaded configuration

Retune mining cadence, miner selection, spam fill, fee floor, and worker pause/resume
state on a running chain without restarting Bitcoin nodes or helper services.

See [RETUNING.md](RETUNING.md), including its
[safe-point behavior](RETUNING.md#safe-point-behavior) and
[configuration ownership](RETUNING.md#configuration-ownership) rules.

### Realistic block and fee pressure

Locally signed raw transactions fill blocks, maintain configurable mempool depth and an
economic fee floor, and can exercise fee replacement without changing Bitcoin Core's
mainnet relay or mempool policy.

See [SETTINGS.md](SETTINGS.md#spammer) for the spam engine and
[the fee market section](SETTINGS.md#the-fee-market-what-spam-pays-and-how-to-set-a-price-floor)
for how a price floor is built without touching relay policy.

## Chain events

### Programmatic reorgs (`invalidateblock`)

Deterministically run one-shot or continuous reorgs with configurable depth, rebuild
replacement blocks from the live mempool, inject new transactions, leave transactions
unconfirmed in chaos mode, or permanently drop selected transactions through simulated
double spends.

See [REORGS.md](REORGS.md#control-plane-reorg-job), plus
[permanent drop](REORGS.md#permanent-drop-double-spend) and
[continuous reorgs](REORGS.md#continuous-reorgs).

### True shorter-chain rewinds

Administratively invalidate the same recent block boundary on node2, node3, and the
production-like node1 endpoint, leaving every node at the same lower height without
mining replacement blocks.

See [REORGS.md](REORGS.md#rewind-without-a-replacement-chain).

### Network splits

Partition the P2P mesh while keeping the RPC control plane reachable, isolating a miner
from its peers without ever touching its RPC interface.

Needs the network agents: start `--profile minimal-organic-reorg` or richer (see
[Profiles](../README.md#profiles)). Details in
[PARTITIONS.md](PARTITIONS.md#deterministic-hard-partition).

### Organic reorgs

Let both sides of a split mine competing branches, then heal the split and observe every
node converge on the most-work chain: a reorg produced by the real mechanism rather than
by an administrative RPC call.

See [PARTITIONS.md](PARTITIONS.md#deterministic-hard-partition), and
[watch both sides live](PARTITIONS.md#watch-both-sides-live) for the two-pane walkthrough.

### P2P link degradation

Add latency and packet loss for a duration or number of blocks, with automatic recovery,
to exercise block and transaction propagation without impairing RPC traffic.

See [PARTITIONS.md](PARTITIONS.md#timed-latency-and-loss).

## Operating the simnet

### Declarative scenario orchestration

Check in YAML scenarios that retune live policy, wait for chain or mempool conditions,
pause/resume mining, fund wallets, mine blocks, run spam bursts, trigger reorgs, create
partitions, degrade links, and expose durable checkpoints for CI.

See [SCENARIOS.md](SCENARIOS.md) for the schema, the
[checkpoint workflow for CI](SCENARIOS.md#checkpoints-and-ci), and the
[shipped examples](SCENARIOS.md#shipped-examples).

### Built-in regtest faucet

Fund one or many application addresses from miner treasury coins through the same
dashboard, CLI, HTTP API, MCP, and scenario job coordinator.

See [CONTROL_PLANE.md](CONTROL_PLANE.md) for the job and its safety invariants, and
[SETTINGS.md](SETTINGS.md#simchain-control-plane) for the reserve and per-request caps.

### Reusable chain snapshots

Named volumes make bootstrap resumable; validated snapshots preserve blocks, chainstate,
miner wallets, the mempool, and the active Compose profile for fast restoration.

See [SNAPSHOTS.md](SNAPSHOTS.md), including
[what survives a snapshot](SNAPSHOTS.md#what-survives-a-snapshot-and-what-doesnt) and its
[risks and edge cases](SNAPSHOTS.md#risks-and-edge-cases).

## Interfaces

All four drive the same control plane, so anything one can do the others can too.

### First-party control dashboard

Watch chain status, retune mining/spam behavior, pause workers, start jobs, inspect job
progress, use the faucet, and jump to the local mempool.space explorer when it is
enabled.

See [CONTROL_PLANE.md](CONTROL_PLANE.md#dashboard).

### CLI interface

Automate control-plane operations from `simchainctl` with stable commands and exit codes
for humans, scripts, and CI.

See [CONTROL_PLANE.md](CONTROL_PLANE.md#cli).

### HTTP API

Drive the same dashboard and job operations through a versioned localhost API with
token-protected mutation routes.

See [CONTROL_PLANE.md](CONTROL_PLANE.md#http-api).

### MCP interface

Let coding agents inspect, retune, and operate the simnet through the control plane's
streamable HTTP MCP endpoint.

See [MCP.md](MCP.md), including [what agents can do](MCP.md#what-agents-can-do) and the
editor setup for [Claude Code](MCP.md#connect-claude-code) and
[Codex](MCP.md#connect-codex).
