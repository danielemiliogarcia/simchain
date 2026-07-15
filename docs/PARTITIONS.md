# Network partitions and P2P degradation

Simchain models network faults through durable control-plane jobs. A hard partition
isolates one miner from Bitcoin P2P while RPC remains reachable, mines deterministic
competing branches, heals the link, and witnesses the expected winning tip on all three
nodes. A degradation job adds bounded latency and/or loss to one node's P2P egress.

Both operations require bootstrap height 204. They share the control plane's single
mutation coordinator, persisted events/results, idempotency behavior, cooperative abort,
and cleanup reporting.

## Start the services

The three namespace-local network agents and control plane are part of the ordinary
stack, exposing the browser, API, MCP, and `simchainctl` contract:

```bash
docker compose up -d --build
```

Each agent shares exactly one node's network namespace with
`network_mode: service:<node>`. It has no host port, independent Docker network, Docker
socket, or public API. Docker drops all capabilities and adds back only `NET_ADMIN`.
The agent discovers the interface routed to `172.30.0.0/24`; RPC and private control
traffic continue over `btc-simnet-control`.

## Deterministic hard partition

```bash
cargo run -p simchainctl -- partition \
  --node node3 --main-blocks 3 --isolated-blocks 4 --wait
```

The convenience script submits the same job:

```bash
./scripts/partition.sh run btc-simnet-node3 --main-blocks 3 --isolated-blocks 4
```

The isolated node may be `node2` or `node3`. Branch lengths must be positive, at most
100, and unequal. With `3` main-side blocks and `4` isolated blocks, the isolated branch
wins; reverse the counts to make the side that remains connected to node1 win.

The coordinator performs these safety steps:

1. verifies bootstrap and equal starting tips;
2. acquires expiring spam and mining pause leases, then rechecks convergence;
3. acquires and renews a hard-partition lease on the target agent;
4. asks Bitcoin Core to disconnect established target peers and witnesses isolation;
5. mines both explicit branch lengths over RPC;
6. clears the impairment, triggers P2P reconnects, and witnesses the expected tip on all
   nodes;
7. releases spam with `chain_changed=true`, then releases mining.

Hard partition rules drop both ingress and egress IP traffic on only the P2P interface.
It is deliberately not modeled as egress loss alone. Existing TCP sessions are flushed
through Bitcoin RPC so the isolation witness does not depend on TCP timeouts.

## Timed latency and loss

```bash
cargo run -p simchainctl -- degrade \
  --node node3 --delay-ms 500 --loss-pct 1 --seconds 60 --wait
```

All three nodes are valid degradation targets. Delay is one-way egress delay; loss is a
percentage from 0 through 100. At least one must be nonzero, and the bounded observation
window is 1–86400 seconds. The convenience wrapper is:

```bash
./scripts/degrade.sh btc-simnet-node3 500 1 60s
```

The old unbounded `netem.sh apply/clear` commands were removed. Every impairment now has
an owning job and lease deadline, so a forgotten shell or dead control plane cannot
leave an indefinite fault behind.

## Browser, HTTP, and MCP

The dashboard's Network section shows current impairments and starts either job. HTTP
uses the same bearer token as every mutation:

```bash
token="$(cat .simchain-control/token)"

curl -s -X POST localhost:8090/api/v1/jobs/partition \
  -H "Authorization: Bearer $token" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: partition-example-1' \
  -d '{"node":"node3","main_blocks":3,"isolated_blocks":4}'

curl -s -X POST localhost:8090/api/v1/jobs/degrade \
  -H "Authorization: Bearer $token" \
  -H 'Content-Type: application/json' \
  -d '{"node":"node3","delay_ms":500,"loss_pct":1,"seconds":60}'
```

MCP exposes `start_partition` and `start_degrade`; inspect progress with `get_job` or
`list_jobs`. CLI/API/MCP/dashboard all reach this one control-plane backend.

## Failure and recovery

Network leases use the same short renewal cadence as worker pause leases. If the control
plane dies, an agent's TTL worker clears nftables/tc state automatically. When the
control plane restarts it marks the old job interrupted, queries all agents for that
job's owner ID, heals and reconnects affected nodes, waits for chain convergence, then
releases spam and mining leases. The mutation lock remains held while any cleanup is
unsafe or incomplete.

Cleanup failures are stored separately from the primary job failure. A terminal success
therefore means both the requested experiment and its safety cleanup succeeded.

## Security and scope

Persistent `NET_ADMIN` in each node namespace is a deliberate tradeoff. It is narrower
than Docker-socket access and cannot create/remove containers or attach networks, but a
compromised agent can alter traffic in its one shared namespace. The internal bearer
token, absence of host ports, dropped capabilities, and `no-new-privileges` constrain
that service.

The impairment targets the Docker P2P interface. Host-side Bitcoin peers connected
through published P2P ports can bypass that path, so disconnect external peers during a
partition experiment. The control-plane coordinator never calls `docker inspect`,
`docker network connect`, or `docker network disconnect` for these operations.

For short operational commands see [RUNBOOK.md](RUNBOOK.md); relevant timeouts are in
[SETTINGS.md](SETTINGS.md).
