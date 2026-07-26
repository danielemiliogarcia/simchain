# Node1 production-like RPC whitelist

## Status

Implemented with Bitcoin Core's native per-user `rpcwhitelist`. There is no proxy,
gateway, or Rust service.

The public configuration is intentionally one boolean:

```env
FILTER_NODE1_RPC=true
```

- `true` enables the complete node1 filter.
- `false` leaves node1 RPC unrestricted.
- Changing it requires recreating node1 and its namespace-sharing network agent.

Node2 and node3 are always unrestricted, even though all nodes share
`BTC_RPC_USER` and `BTC_RPC_PASS`.

## Implementation

Bitcoin Core accepts one comma-separated allowlist:

```text
-rpcwhitelist=<BTC_RPC_USER>:method1,method2,...
```

The fixed Core 31.1 allowlist is isolated in
`docker/node1-rpc-configs.compose.yml`. That included Compose model declares two
configuration objects:

1. `node1-rpc-true` contains `rpcwhitelistdefault=0`, the complete explicit public
   allowlist, and the internal `rpcauth` identity;
2. `node1-rpc-false` contains only the internal `rpcauth` identity.

Node1 selects `node1-rpc-${FILTER_NODE1_RPC}` and always starts with the ordinary
`-conf=/etc/bitcoin/node1-rpc.conf` argument. Compose interpolates `BTC_RPC_USER`
directly into the filtered config content because Core requires the authenticated
username in each `rpcwhitelist` entry. Values other than lowercase `true` or `false`
refer to an undefined config and are rejected while rendering the Compose model.

The Bitcoin image's original entrypoint remains configured. There is no conditional
shell, temporary file, placeholder substitution, bind-mounted policy fragment, or
custom image. Compose `content` interpolation requires Docker Compose 2.23.1 or newer.

The fixed `simchain-internal` identity is full-access and is used by the control plane
to coordinate true shorter-chain rewinds. Its username/password are passed as internal
Compose-defaulted environment wiring only. They intentionally do not appear in either
example environment file and are not supported settings. The public user remains
restricted because it has an explicit whitelist entry; the unlisted authenticated
internal user receives Core's default full permission set. This is a realism guardrail,
not an adversarial security boundary against someone who controls the host.

## Denied methods

When `FILTER_NODE1_RPC=true`, these test/regtest controls are denied:

| Methods |
|---|
| `generate`, `generateblock`, `generatetoaddress`, `generatetodescriptor`, `mockscheduler`, `setmocktime`, `syncwithvalidationinterfacequeue` |

These node-administration methods are also denied:

| Methods |
|---|
| `abortprivatebroadcast`, `clearbanned`, `disconnectnode`, `getblockfrompeer`, `invalidateblock`, `preciousblock`, `prioritisetransaction`, `pruneblockchain`, `reconsiderblock`, `setban`, `setnetworkactive`, `stop`, `submitblock`, `submitheader` |

Bitcoin Core rejects a denied singleton or any batch containing a denied member with
an empty HTTP 403 response. Batch members are checked before any member executes.

## Intentional exceptions

Connectivity methods `addnode` and `addpeeraddress` remain allowed. This keeps the
three-node test network and partition healing simple without a second credential.
The P2P interface itself is never filtered.

These advanced testing methods also remain allowed:

| Method | Testing use / accepted consequence |
|---|---|
| `addconnection` | Open a low-level outbound connection. |
| `sendmsgtopeer` | Send a caller-provided test P2P message. |
| `echo`, `echojson`, `echoipc` | Exercise RPC/IPC argument handling. |
| `logging` | Change runtime diagnostic categories. |
| `dumptxoutset`, `loadtxoutset` | Export or load supported UTXO snapshots. |
| `savemempool`, `importmempool` | Persist or import mempool contents. |

They may change node-local state; that is intentional for an advanced test network.

## Scope

- Only node1 RPC is filtered.
- Node1's host/port, credentials, wallet setting, P2P, ZMQ, and datadir are unchanged.
- Node2/node3 mining, reorg, and administrative RPCs are unchanged.
- Blocks mined by node2/node3 still reach node1 over P2P.
- Bootstrap still mines user-funding blocks 3 and 4 on node2/node3 and matures the
  two 50 BTC coinbases by height 204.
- The policy is an application-facing realism feature, not an operator security
  boundary for the whole simulation.

## Snapshot compatibility

`scripts/snapshot.sh` queries only allowed read methods, then uses
`docker compose stop`. It does not call node1's denied `stop` RPC. Clean bitcoind
shutdown writes `mempool.dat`, and the existing `stop_grace_period: 300s` remains.

An end-to-end live test confirmed that a known unconfirmed transaction survived save,
restart, full volume restore, and policy reapplication.

## Verification

Static checks:

- `scripts/check-node1-rpc-policy.sh` validates the 149-method config allowlist,
  all 21 denied methods, all 12 intentional exceptions, the internal identity, and its
  deliberate absence from both example environment files.
- `scripts/check-compose-security.sh` verifies only node1 receives the boolean,
  selected config, and ordinary `-conf` argument.

Running-stack check:

```bash
./scripts/check-node1-rpc-policy-live.sh
```

It verifies:

- normal reads and raw-transaction RPCs reach node1;
- every denied method returns an empty HTTP 403;
- the internal identity reaches ordinary RPC dispatch for public-forbidden methods;
- every intentional exception reaches Core;
- wallet paths, named parameters, and whitespace cannot bypass filtering;
- a mixed allowed/denied batch executes nothing;
- node2 really mines a block with `generatetoaddress` using the shared credentials;
- node1 receives that exact block over P2P.

The implementation was also exercised through bootstrap, partition/heal,
snapshot/restore, electrs, and mempool.space.

## Upgrade rule

The config allowlist targets Bitcoin Core 31.1. When changing `BTC_IMAGE`, compare
the new Core RPC inventory, decide whether new methods should be public, update the
single allowlist, and rerun both policy checks.
