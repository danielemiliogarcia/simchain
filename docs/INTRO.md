# Introduction to BTC Simchain

## Project Objective

The objective of this project is to be a tool that helps the user write blockchain regtest tests in such a way that the code later needs only minimal modifications (or only configuration, in the best case) to switch to testnet or mainnet.

## Network Components

The network consists of 3 well-connected nodes plus helper containers. The nodes attach
to two Docker networks: `btc-simnet-p2p` carries only node-to-node Bitcoin traffic, and
`btc-simnet-control` carries RPC, health checks, and helper traffic. Workers and the
control plane attach only to the control network; namespace agents share their node's
network namespace. P2P uses the explicit aliases `node1-p2p`, `node2-p2p`, and
`node3-p2p`, so namespace-local agents can impair only P2P traffic without making RPC
unreachable.

### Node 1 `btc-simnet-node1`

Exposed to the host (RPC 18443). Simulates a production endpoint (`-txindex`, `-disablewallet=1`): like most 3rd-party production nodes there is no hot wallet online, so you manage your own keys in an external wallet, obtain the outpoints of your addresses' UTxOs and submit externally signed raw transactions; mining is not under your control. It never mines. Set `NODE1_DISABLE_WALLET=0` in `.env` if you need a wallet on it. Publishes all ZMQ topics on host ports 28332-28336 (see [ZMQ notifications](../README.md#zmq-notifications)).

### Node 2 `btc-simnet-node2`

Exposed to the host (RPC 28443). Simulates an owned node with internal wallet enabled, useful to stack an ordinals wallet or any layer-2 node on top that needs internal wallet management. Publishes all ZMQ topics on host ports 38332-38336, so ZMQ consumers like LND/CLN can use it as their bitcoind backend. This node is a miner.

### Node 3 `btc-simnet-node3`

NOT exposed to the host. Simulates a node connected via p2p but inaccessible to the user. This node is a miner.

### Mining Controller `btc-simnet-mining-controller`

Bootstraps the chain: block 1 goes to node2's wallet, block 2 to node3's wallet, blocks 3 and 4 fund the user address (2 UTxOs of 50 BTC = 100 BTC), then two 50-block funding batches (to node2 then node3) and two 50-block maturity batches, ending at height **204**. Because coinbase maturity is 100 blocks and node3 is funded last (heights 55-104, maturing 155-204), burying to 204 leaves **both miner wallets fully liquid at handoff** (~51 mature coinbases, ~2550 BTC each) so the spammer never starves; the maturity batches keep maturing during the run (heights 205-304). After that the miner nodes produce blocks with bounded exponential timing by default (15-second underlying mean, clamped to 10–20 seconds) and strict miner alternation. Timing can be switched to fixed and miner selection can be weighted independently. Stop this container after funding if you want to control mining manually.

### Spammer `btc-simnet-spammer`

Fills blocks so they are not empty. By default (raw engine) it can run in DATA/HYBRID mode — OP_RETURN data txs of varied sizes that fill blocks at near-zero node cost, kept `SPAM_FILL_BLOCK_RATIO` blocks deep — or in OUTPUT mode, spamming `SPAM_FIXED_TXS_PER_BLOCK` burn-output txs per block. Outputs are paid to burn addresses so no wallet fills with dust. In DATA/HYBRID mode it also maintains a standing pool of `SPAM_FLOOR_POOL_TXS` standalone ~110-vB floor-priced fills, so blocks pack ~100% full and the `FALLBACK_FEE` price floor is **airtight**: a below-floor tx waits in the mempool until it outbids the floor, like mainnet under congestion. See SETTINGS.md "Spammer". On startup it waits for funds to mature and splits them into `SPAM_FANOUT_UTXOS` independent UTXOs, otherwise the 25-tx unconfirmed-chain mempool limit would cap spam at 25 txs per wallet per block. If you spam many transactions, some may stay in the mempool and join the next batch, tune the settings to achieve the scenario you need, or disable with `ENABLE_SPAM=false`. With `ENABLE_SPAM_REPLACES=true` every spam tx signals RBF and a few per batch get fee-bumped, so the mempool carries real BIP125 replacements (see SETTINGS.md).

### Reorg Simulator `btc-simnet-reorg`

A Rust tool (same stack as the other tools, pure RPC calls) that forces chain reorganizations. See [Simulating reorgs](../README.md#simulating-reorgs).

### Control plane `btc-simnet-control-plane`

The single public dashboard/API/MCP backend. It stores desired runtime policy and job
history in the `btc-simnet-control-state` Docker volume, reconciles resident workers through authenticated
private APIs, and runs bounded reorg/scenario/network jobs through Bitcoin RPC and
leases. It is part of ordinary startup, publishes only localhost port 8090, contains no
Docker CLI, drops all Linux capabilities, uses a read-only root filesystem, and mounts
neither the repository nor the Docker socket.

### Partition and network agents

The control plane produces organic competing branches by leasing a namespace-local
network agent on node2 or node3, blocking P2P ingress and egress, mining both sides
explicitly, healing, and waiting for the deterministic winner on every node. The same
agents apply bounded P2P-only delay/loss jobs. They have only `NET_ADMIN`, no host port
or Docker socket, and clear an impairment when its lease TTL expires. Both operations
are post-bootstrap; the funding sequence must reach height 204 first.

### Tools (Profiles)

[mempool.space](https://github.com/mempool/mempool) explorer and/or [electrs](https://github.com/mempool/electrs). See [Profiles](../README.md#profiles).
