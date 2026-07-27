# Network topology

The [README](../README.md#network-topology-overview) shows the same picture with each
Docker network collapsed into a single box. This document expands those boxes into every
container and every link between them.

Traffic is split across two Docker networks. Only the three bitcoind nodes join
`btc-simnet-p2p`, where `node1-p2p`, `node2-p2p`, and `node3-p2p` form the full P2P
mesh on port 18444. Nodes, workers, and the control plane also join
`btc-simnet-control` for RPC, private APIs, health checks, and explorer traffic;
namespace-local agents share their node's two interfaces. This separation lets P2P
links be partitioned or impaired without losing control access. The user talks to
**node1** over RPC on `localhost:18443`; node2's RPC is also exposed on
`localhost:28443`.

## Nodes, workers, and the control plane

```mermaid
flowchart TB
    subgraph host["Host machine"]
        user["User / your tests<br/>external wallet, signs raw txs"]
        zmqc["ZMQ consumers<br/>LND / CLN / indexers / watchers"]
    end

    subgraph mesh["btc-simnet-p2p — bitcoind full mesh (port 18444)"]
        n1["node1 — full node, never mines<br/>txindex, wallet disabled<br/>production-like endpoint"]
        n2["node2 — miner<br/>wallet enabled, owned node"]
        n3["node3 — miner<br/>not exposed to host"]
    end

    subgraph control["btc-simnet-control — RPC and helper traffic"]
        cp["control-plane<br/>dashboard + API + MCP + jobs"]
        mc["mining-controller<br/>bootstrap + configurable mining"]
        sp["spammer<br/>fills blocks with txs"]
        na["3 namespace-local network agents<br/>leased P2P tc/nft only"]
        rg["reorg simulator<br/>profile: reorg, on demand"]
    end

    %% Invisible waypoints pull the host arrows apart so each port label
    %% sits near the host boxes in open space instead of tangling with the
    %% other arrows.
    zmq1(( )):::waypoint
    rpc2a(( )):::waypoint
    rpc2(( )):::waypoint
    zmq2(( )):::waypoint

    user ==>|"RPC localhost:18443"| n1
    user -->|"UI / API / MCP localhost:8090"| cp
    zmqc -.-|"ZMQ 28332-28336"| zmq1
    zmq1 -.-> n1

    user -.- rpc2a
    rpc2a -.-|"RPC localhost:28443"| rpc2
    rpc2 -.-> n2
    zmqc -.-|"ZMQ 38332-38336"| zmq2
    zmq2 -.-> n2

    n1 <-->|P2P| n2
    n1 <-->|P2P| n3
    n2 <-->|P2P| n3

    mc -->|"RPC: mine block"| n2
    mc -->|"RPC: mine block"| n3
    sp -->|"RPC: watch height"| n1
    sp -->|"RPC: raw spam + floor fills"| n2
    sp -->|"RPC: raw spam + floor fills"| n3
    cp -->|"private policy + lease API"| mc
    cp -->|"private policy + lease API"| sp
    cp -->|"Bitcoin RPC jobs"| n1
    cp -->|"Bitcoin RPC jobs"| n2
    cp -->|"Bitcoin RPC jobs"| n3
    cp -->|"private impairment leases"| na
    na -.->|"P2P interface only"| n1
    na -.->|"P2P interface only"| n2
    na -.->|"P2P interface only"| n3
    rg -->|"RPC: invalidate + re-mine"| n3
    rg -.->|"witness poll"| n1

    classDef waypoint width:0px,height:0px,fill:none,stroke:none
```

Not every container above is present in every stack: the control plane, the network
agents and the reorg simulator each arrive with a profile. See
[Profiles](../README.md#profiles) for which tier starts what.

## Explorer stack

With the `electrs` / `mempool` / `all-tools` [profiles](../README.md#profiles), the
explorer stack also joins the network and indexes the chain through node1:

```mermaid
flowchart LR
    browser["Browser<br/>localhost:1080"]
    electrum["Electrum clients<br/>localhost:60001"]

    subgraph net["btc-simnet-control (tool profiles)"]
        mweb["mempool-web"]
        mapi["mempool-api"]
        mdb["mempool-db<br/>MariaDB"]
        el["electrs"]
        n1["node1"]
    end

    browser --> mweb --> mapi
    electrum -.-> el
    mapi -->|"electrum :60001"| el
    mapi -->|"core RPC :18443"| n1
    mapi --> mdb
    el -->|"RPC :18443"| n1
```
