# True shorter-chain rewind plan

Status: proposed; not implemented.

## Goal

Add a bounded **Rewind** action below the existing **Mine blocks** controls. The
operator chooses a positive number of blocks and Simchain moves node1, node2, and
node3 to the common ancestor at `current_height - blocks` without mining a replacement
branch.

This is deliberately different from the existing reorg action:

- a reorg invalidates one branch, mines a higher-work replacement, and lets node1
  adopt that replacement through ordinary chain selection;
- a true rewind ends at a lower height and therefore must apply the same local
  `invalidateblock` decision to all three nodes.

After a successful rewind, all three nodes must report the same lower height and best
block hash. Transactions disconnected from the removed blocks may return to each
node's mempool according to ordinary Bitcoin Core reacceptance policy.

## Important semantic correction

Invalidating blocks on node2 and node3 does **not** make node1 accept their shorter
chain through consensus. Node1 already has a valid chain with more accumulated work
and would keep it. `invalidateblock` is local administrative state and is not
propagated over P2P.

The action must therefore invalidate the same boundary block on node1 as well. Node1
does not "choose" the shorter chain because it has more work; the trusted Simchain
operator explicitly marks the removed suffix invalid on every node. The dashboard and
documentation must call this an administrative test action, not an organic consensus
event.

## Scope and non-goals

In scope:

- one dashboard field and **Rewind** button below the existing mining controls;
- a versioned HTTP job endpoint, shared DTO, service method, CLI command, and MCP tool;
- coordinated `invalidateblock` calls on all three nodes;
- durable progress/recovery and strict postcondition checks;
- a private, least-privilege node1 RPC identity for the control plane;
- preserving the existing public node1 RPC restriction;
- worker leases so background mining and spam cannot race the rewind.

Out of scope:

- deleting block files or pruning data;
- changing proof of work, consensus rules, mock time, or P2P behavior;
- presenting the rewind as a naturally occurring mainnet event;
- exposing `invalidateblock` or `reconsiderblock` to node1's public RPC user;
- Docker socket access, `docker exec`, a second public backend, or a new RPC proxy;
- automatically keeping the chain paused after the job. The job preserves the
  mining worker's previous desired state; users who want to inspect a stable lower
  height should pause mining before pressing **Rewind**.

## Node1 authorization design

The existing public `BTC_RPC_USER` must continue receiving HTTP 403 for
`invalidateblock` and `reconsiderblock` when `FILTER_NODE1_RPC=true`. The control plane
currently uses that same identity, so the rewind creates the first genuine need for a
separate node1 operator identity.

Use Bitcoin Core's native `rpcauth` plus `rpcwhitelist`; do not add a gateway:

```ini
rpcauth=simchain-control:<salt>$<hash>
rpcwhitelist=simchain-control:getbestblockhash,getblockcount,getblockhash,getblockheader,getchaintips,getrawmempool,invalidateblock,reconsiderblock
```

The operator whitelist is intentionally narrow. It permits the reads required for
preconditions, postconditions, and recovery, plus only the two chain-choice mutations.
It does not permit mining, shutdown, network administration, wallet operations, or
arbitrary RPC access.

Configuration rules:

1. Add a dedicated operator username, plaintext password for the control plane, and
   matching precomputed `rpcauth` value. Provide development defaults and document how
   to regenerate the hash when changing the password.
2. Interpolate the `rpcauth` and operator whitelist into both Compose-managed node1
   configs in `docker/node1-rpc-configs.compose.yml`.
3. In filtered mode, keep `rpcwhitelistdefault=1`, the current public allowlist, and
   the narrow operator allowlist.
4. In unfiltered mode, set `rpcwhitelistdefault=0`: the public user remains unrestricted
   while the operator identity is still restricted to its narrow list.
5. Give the control plane the operator username/password through its existing
   environment boundary. Do not mount node1's datadir or cookie into the control plane.
6. Build a dedicated node1 operator RPC client. Do not replace process-global
   `BTC_RPC_USER` credentials, because all existing node1 reads and node2/node3 actions
   must retain their current identities.
7. Keep the control plane with one narrow state volume, no Docker socket, no Docker
   CLI, no repository mount, and no process executor.

The operator credentials are an authorization separation for a local regtest tool, not
a hardened secret from someone who controls the host or can inspect containers. That
matches the repository's existing development credential model.

## Public contract

Add a shared request DTO:

```rust
pub struct RewindJobRequest {
    pub blocks: u64,
}
```

Contracts:

```text
POST /api/v1/jobs/rewind
simchainctl rewind --blocks 3 --wait
MCP tool: rewind_chain { blocks: 3 }
```

Add `JobKind::Rewind` with stable JSON spelling `rewind`. The job is an exclusive,
chain-sensitive mutation. It must not overlap reorgs, partitions, scenarios, faucets,
spam bursts, other rewinds, or network degradations. Follow the current compatibility
matrix rather than introducing a separate coordinator.

Validation:

- `blocks` must be in `1..=100`;
- all nodes must be reachable and start on the same height and best hash;
- bootstrap must be complete;
- `target_height = current_height - blocks` must be at least bootstrap height 204;
- reject while a prior chain mutation is in recovery;
- reject rather than silently clamping an excessive depth.

The 204 floor protects miner wallets, mature bootstrap funding, and assumptions used by
the control plane and scenarios. A future explicit "rewind bootstrap" mode would need a
separate lifecycle design.

## Domain boundary

Create a narrow domain port, for example `RewindBackend`, instead of putting raw RPC
calls in HTTP handlers or dashboard code:

```rust
trait RewindBackend: Send + Sync {
    fn snapshot(&self) -> Result<RewindSnapshot>;
    fn invalidate_to(&self, node: RewindNode, boundary: BlockHash) -> Result<NodeTip>;
    fn reconsider_from(&self, node: RewindNode, boundary: BlockHash) -> Result<NodeTip>;
    fn tips(&self) -> Result<BTreeMap<RewindNode, NodeTip>>;
}
```

The production adapter uses:

- existing normal RPC credentials for unrestricted node2 and node3;
- the dedicated operator client only for node1 mutations;
- ordinary public credentials for non-privileged node1 reads where practical.

Tests use an in-memory backend that can fail before or after each node mutation and can
simulate restart recovery. API, CLI, MCP, and dashboard adapters all call the same job
service.

## Execution algorithm

### Phase 1: reserve and quiesce

1. Normalize and validate the request before reserving a job.
2. Reserve the exclusive mutation lane and persist the normalized request.
3. Acquire the spam lease, then the mining lease, in the same global order used by
   reorg jobs. This prevents wallet/spam mutation and block production during the
   critical section.
4. Start the existing owned lease renewer.
5. Re-read all three tips after both workers acknowledge their safe points.
6. Require identical starting height and hash. Record peer counts for diagnostics, but
   do not require a topology change.

### Phase 2: prepare durable recovery context

Given starting height `H` and depth `N`, resolve and persist before the first mutation:

```text
original_height = H
original_tip = hash(H)
target_height = H - N
target_tip = hash(H - N)
boundary_hash = hash(H - N + 1)
per_node_state = pending | invalidated | restored
```

`invalidateblock(boundary_hash)` invalidates that block and every descendant, so only
one mutation call per node is required even when `N > 1`.

Store this as a rewind-specific recovery context in durable job state. Increment the job
store schema once, with migration defaults for all older jobs. Persist each per-node
transition immediately after verifying that node's new best hash.

### Phase 3: coordinated invalidation

Invalidate in this order:

1. node2;
2. node3;
3. node1 through the operator identity.

Keeping node1 last avoids showing the user-facing endpoint at the lower height while a
miner still advertises the old active tip. After every call:

- verify height equals `target_height`;
- verify best hash equals `target_tip`;
- persist that node as `invalidated`;
- emit a progress event with no credentials or raw auth material.

Do not use parallel RPC calls. The few milliseconds saved are not worth making failure
ordering and durable recovery ambiguous.

### Phase 4: postconditions and release

After node1 is invalidated:

1. poll all three nodes until every height/hash equals the target;
2. record mempool sizes for observability, without requiring mempool contents to be
   identical;
3. mark the durable recovery context resolved;
4. stop the renewer and release spam/mining leases;
5. finish the job as succeeded with the original and final chain snapshots.

Suggested result:

```json
{
  "rewound_blocks": 3,
  "original_height": 850,
  "original_tip": "...",
  "final_height": 847,
  "final_tip": "...",
  "boundary_hash": "...",
  "nodes": {
    "node1": {"height": 847, "best_hash": "...", "mempool_size": 120},
    "node2": {"height": 847, "best_hash": "...", "mempool_size": 121},
    "node3": {"height": 847, "best_hash": "...", "mempool_size": 119}
  },
  "mining_desired_state_changed": false
}
```

Mempool counts may differ because transaction reacceptance and local policy are
node-local. Equal chain tips are the authoritative success condition.

## Abort and failure behavior

Before the first successful `invalidateblock`, abort normally and release the leases.

After any node has changed chain, abort is no longer an immediate stop. The job must
first restore a safe, common state:

1. call `reconsiderblock(boundary_hash)` on each node already invalidated;
2. verify all nodes return to `original_tip`;
3. only then report `aborted_safely` and release leases.

For an ordinary RPC failure mid-rewind, use the same rollback-to-original path. If
rollback cannot be verified, keep the job in recovery, retain or reacquire worker
leases, and block incompatible mutations. Never report success from a partial rewind.

Errors must distinguish:

- `rewind_precondition_failed`;
- `rewind_invalidation_failed`;
- `rewind_rollback_failed`;
- `rewind_convergence_failed`;
- `rewind_recovery_required`.

## Restart recovery

A control-plane crash may occur after only some nodes have invalidated the boundary.
On startup:

1. detect unresolved rewind recovery context before admitting new mutation jobs;
2. reacquire spam and mining leases as early as the existing recovery framework allows;
3. inspect current ancestry and tips on every node;
4. if every node is already on `target_tip`, preserve the completed rewind and mark the
   interrupted job safely resolved;
5. otherwise prefer restoring all nodes to `original_tip` with `reconsiderblock`;
6. if background mining resumed after lease expiry, identify whether current tips
   descend from the target or original branch before mutating anything;
7. converge to one verified safe tip, record the recovery result, then unlock the
   coordinator.

Recovery must never assume that a persisted "RPC requested" flag means the RPC took
effect. Every decision comes from current node ancestry and best-hash observations.
If neither the original nor target state can be reconstructed automatically, keep the
coordinator blocked and expose an actionable recovery error instead of guessing.

## Dashboard design

Replace the visually empty lower half of the current mining card with two internal
subpanels:

```text
Mine blocks
  Node:   [node2 v]
  Blocks: [1]
  [Mine]

Rewind chain
  Blocks: [1]
  [Rewind]
```

Behavior:

- keep the existing Mine form unchanged;
- Rewind has no node selector because it always targets all three nodes;
- default to one block, with `min=1` and `max=100`;
- use a secondary/danger visual treatment rather than the ordinary yellow primary
  action style;
- require an explicit confirmation dialog stating the current and target heights;
- explain that all three nodes are administratively invalidated and disconnected
  transactions may return to mempools;
- explain that active mining resumes after the bounded job unless the user paused it;
- show durable job progress and result through the existing job viewer;
- mirror backend compatibility for button disablement, while keeping backend
  reservation authoritative against races.

The UI must not claim "node1 follows by consensus." Suggested help text:

> Administratively invalidate the newest blocks on all three nodes and converge at a
> lower common height. This is a test-only rewind, not a proof-of-work reorg.

## Security and policy assertions

Extend static and live checks:

- the public node1 user still receives empty HTTP 403 for `invalidateblock` and
  `reconsiderblock`;
- the operator identity is accepted only by node1 and only for its explicit narrow
  whitelist;
- the operator identity cannot call `generatetoaddress`, `stop`, `setban`, wallet RPCs,
  or unrelated administrative methods;
- node2 and node3 remain unrestricted under the existing shared credentials;
- changing `FILTER_NODE1_RPC` does not accidentally broaden the operator whitelist;
- the rendered Compose model contains no shell wrapper, Docker socket, node datadir
  mount in the control plane, or exposed private service port;
- logs, events, job request JSON, and status payloads never contain operator passwords
  or `rpcauth` material.

## Tests

### Pure/unit tests

- request normalization rejects zero, over 100, and below-bootstrap targets;
- target and boundary hashes are computed correctly for depths 1 and 100;
- one boundary invalidation represents the full requested suffix;
- job compatibility classifies Rewind as exclusive and convergence-sensitive;
- public and operator allowlists contain exactly their intended methods;
- recovery state transitions are exhaustive and serializable;
- old job-store schemas migrate with no invented rewind recovery context.

### Job tests with fault injection

- success invalidates node2, node3, then node1 and releases both worker leases;
- failure before mutation changes no chain state;
- failure after node2 restores node2 to the original tip;
- failure after node3 restores both miners;
- failure after node1 but before final persistence recognizes a completed target;
- abort before mutation exits immediately;
- abort after mutation restores the original chain before returning;
- partial rollback keeps the coordinator blocked in recovery;
- restart recovery handles every per-node mutation boundary;
- mining/spam lease cleanup never runs before a common-tip postcondition.

### API/CLI/MCP/dashboard tests

- authentication, idempotency, stable error envelopes, and job watching;
- the same normalized request reaches every adapter;
- HTML contains the two mining-card subpanels, bounded field, help, confirmation, and
  Rewind button;
- active incompatible jobs disable Rewind in the UI and are rejected by the server;
- no node selector is sent for Rewind.

### Live acceptance

On a fresh bootstrapped chain:

1. pause mining or acquire the bounded job lease;
2. record all three tips at height `H`;
3. submit `rewind --blocks 3 --wait`;
4. require every node at height `H - 3` with the same expected ancestor hash;
5. prove the former boundary block is locally invalid on all three nodes;
6. prove node1 public `invalidateblock` remains HTTP 403;
7. prove a normal node1 read still works;
8. resume/mine one block and require all nodes converge at `H - 2` on a descendant of
   the rewind target;
9. save and restore a snapshot and verify the rewound active chain persists;
10. repeat with injected failures after each node and verify rollback/recovery.

Also run the existing bootstrap, reorg, partition/heal, snapshot, node1 RPC policy,
Compose trust-boundary, explorer, and full Rust CI-equivalent suites.

## Documentation changes during implementation

Update:

- `README.md`: distinguish Mine, Rewind, and Reorg;
- `docs/RUNBOOK.md`: CLI/API examples, pause-mining advice, and recovery guidance;
- `docs/CONTROL_PLANE.md`: endpoint, DTO, job phases, CLI, and MCP tool;
- `docs/SETTINGS.md`: private operator auth and unchanged public RPC policy;
- `docs/SNAPSHOTS.md`: persistent invalid-chain state;
- `docs/PARTITIONS.md`: rewind is forbidden during impairments;
- the node1 RPC whitelist plan: document the new least-privilege internal identity;
- `.env.example` and `.env.full.example`: operator credential/hash relationship;
- `AGENTS.md`: any new backend/test files.

## Implementation sequence

1. Add the shared request/response types and `JobKind::Rewind` without exposing a route.
2. Add the narrow operator `rpcauth` identity to both declarative node1 configs and
   extend static/live policy tests.
3. Add an explicit-auth node1 operator client and `RewindBackend` with mock coverage.
4. Add request validation, target calculation, and converged-start checks.
5. Add durable rewind recovery context and job-store migration.
6. Implement owned spam/mining lease acquisition and coordinated invalidation.
7. Implement abort rollback, restart recovery, and blocked-until-safe semantics.
8. Expose the shared service through HTTP, CLI, MCP, and dashboard subpanel.
9. Add unit, fault-injection, adapter, security, and live acceptance tests.
10. Update documentation and run the complete CI-equivalent plus snapshot/explorer
    verification matrix.

## Acceptance criteria

- Rewinding `N` blocks leaves node1, node2, and node3 at exactly `H - N` on the same
  expected ancestor.
- No replacement blocks are mined by the rewind action.
- Public node1 callers still cannot invoke chain-choice RPCs.
- The control plane uses a separate least-privilege identity only for node1 rewind and
  recovery.
- Partial execution, abort, and restart never silently leave a split chain or unlock
  incompatible mutations before recovery.
- Previous mining/spam desired states are preserved.
- Existing Reorg behavior remains unchanged and is clearly distinguished in the UI.
- The implementation adds no proxy, second backend, Docker privilege, or node datadir
  access to the control plane.
