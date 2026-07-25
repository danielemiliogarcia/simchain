# Node1 production-like RPC whitelist plan

## Status and decision

Use Bitcoin Core's native per-user `rpcwhitelist` on node1's host-facing RPC
endpoint. Do not add a reverse proxy or a new Rust gateway.

The feature models a hosted, production-like Bitcoin RPC endpoint by withholding
regtest block-generation controls and selected node-administration calls from node1.
It is an endpoint-behavior policy, not a consensus or operator security boundary.

The following decisions are settled:

- Only node1 has an RPC whitelist.
- Node2 and node3 remain completely unrestricted, even though all three nodes use the
  same `BTC_RPC_USER` and `BTC_RPC_PASS`.
- Node1's P2P port/interface and ZMQ endpoints are untouched.
- The node1 host, port, username, password, and JSON-RPC payload format remain the
  same for user applications.
- `addnode` and `addpeeraddress` are deliberately allowed on node1 for Simchain
  connectivity simplicity.
- `addconnection`, `sendmsgtopeer`, `echo`, `echojson`, `echoipc`, `logging`,
  `dumptxoutset`, `loadtxoutset`, `savemempool`, and `importmempool` are deliberately
  allowed for advanced tests.
- A custom Simchain error body is not required; Bitcoin Core's native empty HTTP 403
  response is acceptable.
- The two policy groups, `superpowers` and `admin`, must be independently selectable
  by startup configuration.

## Why native `rpcwhitelist`

Bitcoin Core already:

- authenticates the HTTP Basic-auth user before method authorization;
- parses the JSON-RPC body before checking the method;
- compares exact method names rather than matching body text;
- checks every member of a batch before executing any member;
- rejects a disallowed singleton or mixed batch with HTTP 403;
- applies whitelists per username and per bitcoind process;
- intersects multiple whitelist declarations for the same username.

This removes the need to implement and maintain HTTP forwarding, JSON parsing,
wallet-path handling, authorization-header forwarding, batch reconstruction, request
limits, retries, and proxy error compatibility.

The pinned `bitcoin/bitcoin:31.1` image was tested directly during planning:

```text
public getblockcount:       HTTP 200
public setmocktime:         HTTP 403, empty body
unlisted internal user:     HTTP 200 with rpcwhitelistdefault=0
mixed allowed/denied batch: HTTP 403, empty body
```

Relevant upstream references:

- [Bitcoin Core JSON-RPC security and whitelist documentation](https://github.com/bitcoin/bitcoin/blob/master/doc/JSON-RPC-interface.md#security)
- [Bitcoin Core 31.1 whitelist implementation](https://github.com/bitcoin/bitcoin/blob/v31.1/src/httprpc.cpp)
- [Bitcoin Core whitelist functional test](https://github.com/bitcoin/bitcoin/blob/v31.1/test/functional/rpc_whitelist.py)

## Scope and topology

The network topology stays as it is:

```text
host application -- RPC localhost:18443 --> node1 (whitelisted user)

trusted Simchain services -- RPC --> node1 (same whitelisted user)
trusted Simchain services -- RPC --> node2/node3 (unrestricted)

node1 <------------------ P2P ------------------> node2
node1 <------------------ P2P ------------------> node3
node2 <------------------ P2P ------------------> node3
```

Configuring `rpcwhitelist` for `foo` on node1 does not affect `foo` on node2 or
node3. Each bitcoind process owns its own authentication and whitelist tables.

The RPC whitelist does not filter P2P. A valid block may still arrive at node1 over
P2P, exactly as it can at a mainnet node. Node2 also remains intentionally available
for unrestricted mining RPC, so this feature does not attempt to prevent an operator
from changing the simulation chain.

## One shared RPC user

Use the existing `BTC_RPC_USER` and `BTC_RPC_PASS` on all three nodes. Do not add a
second node1-internal RPC identity initially.

This is viable because:

- node1's first-party uses are predominantly observation and ordinary transaction
  RPCs, which are present in its public allowlist;
- node2 and node3 perform block generation and reorg mutation;
- node2 and node3 are not whitelisted;
- node1's one currently required partition-recovery administration call is
  `addnode`, which is an intentional public exception.

If a future first-party feature genuinely requires a node1 method that public callers
must not have, introduce a separate least-privilege user at that time. Do not create
an unrestricted internal user preemptively.

## Intentional connectivity exceptions

`addnode` and `addpeeraddress` are allowed for every authenticated node1 RPC caller.

Reasons:

- Simchain is a local, three-node test network rather than a multi-tenant hosted
  service.
- Allowing peer hints does not manufacture blocks, bypass consensus validation,
  change proof-of-work, mock time, or create spendable funds.
- The control plane currently uses `addnode ... onetry` during partition healing to
  trigger immediate reconnection.
- Allowing `addnode` avoids a second credential solely for connectivity recovery.
- `addpeeraddress` may be useful for experiments that populate addrman and is not a
  block-generation primitive.
- P2P is deliberately not part of the user-facing restriction.

This exception must be visible in `docs/SETTINGS.md` and `docs/PARTITIONS.md`, so a
future security review does not classify these methods as accidental omissions.

Related read methods such as `getpeerinfo`, `getconnectioncount`,
`getaddednodeinfo`, `getaddrmaninfo`, and `getnodeaddresses` remain allowed.

## Intentional advanced-testing exceptions

The strict default policy also allows the following RPCs for advanced Simchain tests:

| Method | Testing use / accepted consequence |
| --- | --- |
| `addconnection` | Opens a low-level regtest outbound connection with an explicit connection type. |
| `sendmsgtopeer` | Sends a caller-supplied P2P test message to an existing peer. |
| `echo`, `echojson`, `echoipc` | Exercises JSON-RPC and IPC argument/transport behavior. |
| `logging` | Changes runtime log categories for diagnosis during a test. |
| `dumptxoutset` | Exports node-local UTXO state and may temporarily roll back/suspend network activity for some modes. |
| `loadtxoutset` | Loads supported assumeutxo state into node1. |
| `savemempool` | Explicitly writes the current mempool to disk. |
| `importmempool` | Imports mempool contents from disk. |

These methods can mutate node-local state, expose test-only behavior, or temporarily
disrupt node1. Their availability is intentional because Simchain is a testing tool
and advanced tests may need them. They still do not directly provide the immediate
regtest mining calls that this feature primarily exists to withhold.

Document them as named exceptions in user settings and upgrade reviews. Starting
permissive here is reversible: a later policy version can move individual methods
into `superpowers` or `admin` after concrete compatibility and migration analysis.

## Peer discovery and bootstrap

The Compose file currently gives all three nodes persistent `-addnode` relationships.
This is deterministic and may remain unchanged. Startup `-addnode` configuration is
not affected by the RPC whitelist.

An optional later topology simplification can make node1 inbound-only:

- remove node1's `-addnode=node2-p2p:18444` and
  `-addnode=node3-p2p:18444` arguments;
- retain node2 and node3's persistent `-addnode` entries pointing to node1 and each
  other;
- let node1 accept inbound connections from both miners;
- initiate immediate post-partition reconnection from node2/node3 only.

Do not use `-seednode` for the default three-node topology. A seed connection obtains
addresses and disconnects; regtest has no useful public DNS/fixed seeds, and Docker
IP address gossip is less deterministic than service-name `-addnode` configuration.

The whitelist feature does not require this topology change. Keep it separate so RPC
policy does not accidentally alter P2P behavior.

Connectivity should be verified at lifecycle boundaries rather than added to the
permanent Docker healthcheck, because intentional partitions legitimately reduce
peer counts:

1. Before bootstrap, optionally require node1 to see both miner peers.
2. After bootstrap, require node1 height 204, equal tips on all nodes, and expected
   peer connectivity.
3. Before a partition, require a converged chain and connected starting topology.
4. After healing, trigger reconnects, restore expected peer connectivity, and require
   all tips to converge.

Use `getpeerinfo` when direction or connection type matters; `getconnectioncount`
only supplies a total.

## Policy groups

The lists target the pinned Bitcoin Core 31.1 image and must be reviewed on every
`BTC_IMAGE` upgrade.

### `superpowers`

These methods expose regtest/testing behavior rather than ordinary hosted-node
behavior and are denied when the group is active:

| Method | Reason |
| --- | --- |
| `generate` | Legacy hidden generation RPC; include defensively. |
| `generatetoaddress` | Immediately mines blocks to an address. |
| `generatetodescriptor` | Immediately mines blocks to a descriptor. |
| `generateblock` | Immediately constructs/mines a caller-selected block. |
| `setmocktime` | Changes node-local time on mockable/regtest chains. |
| `mockscheduler` | Advances the regtest scheduler. |
| `syncwithvalidationinterfacequeue` | Hidden test synchronization hook. |

`addpeeraddress`, `addconnection`, `sendmsgtopeer`, `echo`, `echojson`, and `echoipc`
are intentionally excluded from this group despite Core documenting them as hidden
or testing-oriented.

The phrase "generate addresses" in this feature means block-generation calls such as
`generatetoaddress`. Ordinary wallet `getnewaddress` is realistic wallet behavior and
is not denied by this group; node1 already has `-disablewallet=1` by default.

### `admin`

These methods change shared node state in ways a hosted application RPC normally
would not expose and are denied when the group is active:

| Area | Methods |
| --- | --- |
| Process | `stop` |
| Network | `disconnectnode`, `setban`, `clearbanned`, `setnetworkactive` |
| Chain choice | `invalidateblock`, `reconsiderblock`, `preciousblock` |
| Block/header RPC ingress | `submitblock`, `submitheader`, `getblockfrompeer` |
| Mining policy | `prioritisetransaction` |
| Persistent/local data | `pruneblockchain` |
| Private broadcast | `abortprivatebroadcast` |

`addnode`, `addpeeraddress`, `logging`, `dumptxoutset`, `loadtxoutset`,
`savemempool`, and `importmempool` are intentional exclusions from `admin`.

Blocking `submitblock` and `submitheader` only restricts the user-facing RPC surface;
it does not prevent valid block/header propagation over P2P. Read-only expensive
methods and wallet methods are outside this initial policy.

## Allowlist construction

Bitcoin Core provides an allowlist, not a denylist. For the selected policy, compute:

```text
all RPC methods supported by pinned Bitcoin Core
  minus enabled superpowers methods
  minus enabled admin methods
  plus documented connectivity and advanced-testing exceptions
  equals NODE1 public allowlist
```

Every allowed method must be named explicitly. This means a newly added Bitcoin Core
RPC is denied until reviewed, which is a safe upgrade failure mode but differs from
the original pass-future-methods gateway idea.

Maintain one canonical versioned method inventory and derive four tested presets:

| Policy | Superpowers denied | Admin denied |
| --- | --- | --- |
| `superpowers,admin` | yes | yes |
| `superpowers` | yes | no |
| `admin` | no | yes |
| `none` | no | no |

Desired configuration interface:

```env
NODE1_RPC_FILTER_GROUPS=superpowers,admin
```

Valid values are `superpowers,admin`, `superpowers`, `admin`, and `none`.

Bitcoin Core does not understand group names or negative method lists. The
implementation therefore needs a startup-only policy resolver that selects the
corresponding precomputed whitelist and appends one `-rpcwhitelist` argument before
starting bitcoind. This may be a small validated shell wrapper around the official
image entrypoint; it is configuration glue, not a network service. Keep the method
inventory and set calculation independently testable, and do not duplicate four
manually maintained long strings.

If avoiding even a startup wrapper is preferred, expose the complete allowlist as a
single environment value instead. That is operationally simpler to implement but
makes group selection and review substantially less readable. Resolve this narrow
implementation choice before coding; it does not change the native whitelist design.

Use `rpcwhitelistdefault=1` explicitly so any future authenticated user without an
explicit whitelist can execute no RPC methods. Today the configured shared user is
the only intended authenticated node1 user.

## Compose changes

On node1 only:

- retain the current RPC host port and credentials;
- add `-rpcwhitelistdefault=1`;
- add the resolved `-rpcwhitelist=<BTC_RPC_USER>:<allowed methods>`;
- add policy selector configuration;
- bind the host RPC publication to `127.0.0.1` unless remote-host access is an
  explicitly supported requirement;
- leave P2P, ZMQ, volumes, wallet setting, and internal RPC port unchanged.

On node2 and node3:

- do not add `rpcwhitelist` or `rpcwhitelistdefault`;
- retain unrestricted mining/reorg/admin RPC behavior;
- retain all existing P2P behavior and host publications.

## Snapshot compatibility

`scripts/snapshot.sh` does not call the `stop` or `savemempool` RPC on node1 (or on
the other nodes). Its save path:

1. calls only `getblockcount` and `getbestblockhash` on node1;
2. records the running Compose services;
3. runs `docker compose stop` for the stack;
4. lets Docker deliver the normal termination signal to each bitcoind;
5. relies on bitcoind's clean shutdown to flush chainstate, wallets, and
   `mempool.dat`;
6. archives all three stopped datadir volumes.

Restore uses only the allowed node1 methods `getblockcount` and `getblockhash` for
verification. Therefore `stop` and `savemempool` can remain absent from node1's
strict public allowlist without affecting snapshots.

Node services currently have `stop_grace_period: 300s`, allowing large mempool and
chainstate flushes to finish before Docker may kill the process. Any startup-only
whitelist resolver must finish by using `exec` to replace itself with the official
Bitcoin image entrypoint/bitcoind process; it must not remain as a PID 1 shell that
swallows or mishandles SIGTERM. Preserve the existing entrypoint's datadir and user
handling as well.

Add an end-to-end snapshot regression test:

- leave a known transaction unconfirmed in node1's mempool;
- save a snapshot under the strict default whitelist;
- verify that the save needed no `stop` or `savemempool` RPC permission;
- restore it and verify the transaction was reloaded from `mempool.dat`;
- confirm the saved-height block hash and all node datadirs still restore correctly.

Update the snapshot documentation while implementing this feature: its risk section
still mentions an older 10/60-second grace period even though Compose now grants each
node 300 seconds.

## Verification

### Static policy tests

- The canonical Core 31.1 inventory contains every expected supported method.
- Each preset equals the inventory minus exactly its enabled groups.
- `addnode` and `addpeeraddress` are present in every preset, including the strict
  default.
- Every documented advanced-testing exception is present in every preset, including
  the strict default.
- Every `superpowers` method is absent when that group is active.
- Every `admin` method except the two documented connectivity exceptions is absent
  when that group is active.
- Unknown group names and malformed selector values fail startup.
- A Core-version upgrade requires an explicit inventory/policy review.

### Live node1 tests

- `getblockcount`, `getpeerinfo`, ordinary raw-transaction calls, every connectivity
  exception, and every advanced-testing exception pass through Core authorization.
- `generatetoaddress`, `generateblock`, `setmocktime`, `invalidateblock`, and `stop`
  return HTTP 403 under the default policy.
- A batch containing one allowed and one forbidden method returns 403 and executes
  neither method.
- Whitespace, named parameters, positional parameters, member ordering, and wallet
  paths cannot bypass method authorization.
- The four group selector values exhibit the intended matrix.

### Cross-node and regression tests

- The same username can call `generatetoaddress` on node2 and node3.
- Node2 and node3 have no rendered whitelist settings.
- Bootstrap reaches height 204 and node1 converges.
- Partition start, isolation, healing, and reconnect still work with public node1
  `addnode` permission.
- P2P and ZMQ ports and behavior are unchanged.
- Optional electrs/mempool profiles continue to index node1 using allowed methods.

Extend `scripts/check-compose-security.sh` to assert that only node1 has whitelist
arguments and that all documented connectivity and advanced-testing exceptions remain
in the strict rendered policy.

Run the repository's CI-equivalent checks:

```bash
cargo ba && cargo ca && cargo fac && cargo tt
./scripts/check-compose-security.sh
./scripts/check-docker-images.sh
```

## Documentation updates

- README: describe node1 as wallet-disabled and RPC-whitelisted; node2/node3 remain
  unrestricted.
- `docs/INTRO.md`: explain that the production-like boundary applies only to node1
  RPC and does not filter P2P.
- `docs/SETTINGS.md`: document the group selector, exact group membership, 403
  behavior, Core-version review requirement, shared-username semantics, and every
  intentional advanced-testing exception with its side effects.
- `docs/PARTITIONS.md`: explicitly state that `addnode` and `addpeeraddress` are
  public node1 exceptions for deterministic connectivity and recovery.
- `docs/RUNBOOK.md`: add examples proving a denied node1 generation call and an
  allowed node2 generation call with the same credentials.

## Delivery phases

1. Build the canonical Core 31.1 method inventory and policy-set tests.
2. Resolve and implement the startup selector/whitelist injection mechanism.
3. Add node1-only Compose configuration and rendered-model assertions.
4. Run live authorization, batch atomicity, bootstrap, partition, and optional-profile
   compatibility tests.
5. Update user and operator documentation, including every intentional connectivity
   and advanced-testing exception.

## Acceptance criteria

- There is no proxy or new network-facing service.
- Node1 alone enforces a native Bitcoin Core RPC whitelist.
- Node2 and node3 remain unrestricted with the same username/password.
- Both policy groups can be selected independently by configuration.
- Default node1 callers cannot invoke block-generation, mock-time, chain-choice, or
  selected administrative methods.
- `addnode` and `addpeeraddress` remain allowed and are explicitly documented as
  intentional Simchain connectivity exceptions.
- `addconnection`, `sendmsgtopeer`, `echo`, `echojson`, `echoipc`, `logging`,
  `dumptxoutset`, `loadtxoutset`, `savemempool`, and `importmempool` remain allowed
  and are explicitly documented as intentional advanced-testing exceptions.
- Mixed JSON-RPC batches containing a forbidden method execute nothing.
- Node1 P2P/ZMQ behavior and all node2/node3 behavior remain unchanged.
- Bootstrap, partitions, healing, scenarios, faucet, spam, reorgs, snapshots, and
  optional explorer profiles continue to work.
