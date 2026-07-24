# Parallel network degradation jobs plan

Status: implemented. The filename intentionally follows the requested `paralell`
spelling. Job-store schema v4 keeps one exclusive lane plus per-node degradation lanes;
the shared API and dashboard expose all active lanes, and restart recovery clears each
job's resources independently.

## Goal

Allow a timed P2P degradation job to remain active while a bounded manual mine job runs,
so an operator can deliberately create a block and observe delayed propagation.

Keep reorgs, partitions, scenarios, faucets, and other topology/chain-sensitive
operations sequential until each combination has an explicit safety decision.

## Why it is blocked today

The control plane persists one global `active_job_id`. Every mutation job, including
`degrade` and `mine`, reserves that same slot. A degradation owns only a lease on one
namespace-local network agent, while a mine job owns the mining worker lease and calls
Bitcoin RPC, but the coordinator currently does not express those disjoint resources.
The second request is therefore rejected before resource-level compatibility matters.

This is a scheduling limitation, not a Bitcoin or network-agent requirement.

## Do not create a second control plane

A second endpoint backed by another control-plane process or a thread pretending to be
another control plane is the wrong boundary:

- The repository requires one public Simchain backend.
- The durable state directory has an exclusive instance lock.
- Two managers writing the same job index/config state would introduce split-brain
  persistence and unsafe cleanup.
- Duplicating API/MCP/dashboard logic would violate the shared domain-service design.

Keep the existing HTTP endpoint and `JobManager`; add explicit concurrency lanes and
resource compatibility inside it. Jobs already execute on separate threads, so no new
process model is needed.

## Proposed concurrency model

Replace the single active slot with durable lanes:

```rust
struct ActiveJobs {
    exclusive: Option<String>,
    degradations_by_node: BTreeMap<NetworkNode, String>,
}
```

The `exclusive` lane retains the existing one-at-a-time rule for mine, spam burst,
reorg, partition, scenario, faucet, and other chain mutations. Degradation jobs occupy
one slot per target node because each network agent supports one active impairment
lease and one qdisc/ruleset at a time.

Phase-1 compatibility matrix:

| Existing active job | New mine | New degradation | Other exclusive job |
|---|---:|---:|---:|
| none | allow | allow | allow |
| degradation | **allow** | allow only on another node | reject |
| mine | n/a (exclusive lane occupied) | **allow** | reject |
| spam burst | reject initially | reject initially | reject |
| reorg | reject | reject | reject |
| partition | reject | reject | reject |
| scenario | reject | reject | reject |
| faucet | reject | reject | reject |

This delivers the requested degradation-plus-mine experiment without claiming every
combination is safe. Later work may allow degradation plus spam burst for transaction
propagation experiments after dedicated tests.

The matrix must be centralized in a pure function, not scattered through HTTP handlers:

```rust
fn jobs_are_compatible(active: &[ActiveJob], requested: JobKind) -> Compatibility;
```

Reservation and compatibility checks must happen atomically under the manager state
mutex. Never “check then reserve” across separate lock acquisitions.

## Resource reasoning

`degrade + mine` is a safe first pair because:

- Degradation owns `network:<node>` only.
- Mine owns a mining pause lease and performs bounded RPC mining.
- RPC/control traffic uses `btc-simnet-control`, while netem targets only the P2P
  interface.
- Each job has its own abort flag, renewer, events, result, and cleanup.
- Mine cleanup cannot clear a network lease, and degradation cleanup cannot release a
  mining lease.

Same-node operation is intentional and useful: degrading node2's P2P egress while
mining on node2 delays that new block's announcement without delaying the RPC command
that creates it.

Do not allow degradation plus partition initially. They may target different nodes, but
partition convergence deadlines and network cleanup assume an otherwise clear P2P
network. Do not allow degradation plus reorg/scenario initially for the same reason.

## Egress-only demo semantics

The current netem implementation shapes target-node egress only. It does not delay
packets arriving at the target.

For a visible block propagation demonstration, use the same origin for both commands:

```bash
cargo run -p simchainctl -- degrade start \
  --node node2 --delay-ms 5000 --loss-pct 0 --seconds 30 --wait

# While degradation is in observing_degraded_network:
cargo run -p simchainctl -- mine --node node2 --blocks 1 --wait
```

Run the degradation command without `--wait` in the launching terminal, or watch its job
from another pane. Node2 mines immediately over RPC; node1/node3 should learn the block
after the shaped egress delay. Degrading node3 while mining node2 does not necessarily
delay node2-to-node3 delivery because node3 ingress is not shaped.

Document this prominently in the dashboard help and runbook.

## Persistence changes

At planning time, the job store was schema version 2 with one `active_job_id`. The
scenario-settings work first introduced schema v3; this implementation therefore uses
schema v4 for one exclusive active slot plus the per-node degradation map.

Migration:

- A missing old active ID becomes empty lanes.
- An old active degradation is placed in `degradations_by_node`, deriving the normalized
  node from its persisted request.
- Any other old active job is placed in `exclusive`.
- Reject corrupt state containing duplicate active degradations for one node.

Other handoff plans may also need the next job-schema version. Coordinate the version
bump when implementing multiple plans; do not independently land incompatible “v3”
formats. Prefer one migration that adds all fields present in the implementation branch.

The global job event sequence and per-job JSONL files already support interleaved events
and do not need separate stores.

## Restart recovery

Startup currently marks one active job interrupted and spawns one recovery loop. Change
it to enumerate every durable active lane:

1. Mark every nonterminal active job interrupted and cleanup running.
2. Persist all state transitions atomically.
3. Spawn recovery for each job.
4. Degradation recovery clears only leases owned by that degradation job on its target
   agent.
5. Mine recovery releases only its worker lease.
6. Clear each lane independently after its recovery succeeds.
7. Keep incompatible new work blocked while any relevant recovery remains active.

Recovery loops may run in parallel because ownership is disjoint, but all persisted
updates still go through the one manager mutex and atomic job-index save.

## API and status compatibility

Keep existing mutation endpoints:

```text
POST /api/v1/jobs/degrade
POST /api/v1/jobs/mine
```

No special parallel endpoint is necessary. Compatibility belongs to the scheduler and
therefore applies equally to HTTP, CLI, MCP, and dashboard callers.

The current list/status contracts expose singular `active_job_id`/`active_operation`.
Extend them without immediately deleting compatibility fields:

```json
{
  "active_job_id": "exclusive-job-if-present",
  "active_job_ids": ["mine-job", "degrade-job"],
  "active_jobs": [
    {"job_id":"...", "kind":"mine", "lane":"exclusive"},
    {"job_id":"...", "kind":"degrade", "lane":"network:node2"}
  ]
}
```

When only one job is active, preserve the old singular value. With multiple jobs, set
the singular field to the exclusive job when present; otherwise use the oldest active
degradation. Document it as compatibility-only and make new UI logic consume the list.

## Dashboard behavior

The dashboard currently disables every mutation control when `activeMutationId()` is
non-null. Replace that global boolean with action compatibility derived from the active
job list.

Required behavior:

- While degradation is active, enable Mine and keep incompatible controls disabled.
- While Mine is active, permit starting a degradation.
- Disable degradation for a node that already has an active degradation.
- Permit degradation of another node if no incompatible exclusive job is active.
- Show all active jobs, not one banner that hides the other.
- Let the operator select/abort/watch either job independently.
- Explain egress direction in the node and delay help text.

Do not infer compatibility only in JavaScript. The backend must reject invalid races;
the UI mirrors server rules for usability.

## Cleanup and failure independence

- Mine success/failure must not terminate the degradation duration.
- Aborting degradation must not abort Mine.
- Aborting Mine must not heal degradation.
- A degradation lease-renew failure heals only its target and marks that job failed.
- If Mine changes the chain and later cleanup fails, degradation still owns its lease
  until its own terminal path.
- Job results and cleanup errors remain attached to their individual job IDs.

The general lease cleanup helper currently accepts arbitrary lease lists. Add assertions
or typed lane/resource helpers so one job cannot release a lease whose owner job ID does
not match.

## Implementation sequence

1. Define the compatibility matrix as pure domain logic with exhaustive `JobKind` tests.
2. Add `ActiveJobs` to persistence and migrate the old singular active slot.
3. Refactor reservation, terminal completion, abort, list/status, history trimming, and
   recovery to address lanes rather than one ID.
4. Classify Mine as exclusive-but-degradation-compatible and Degrade as per-node network
   lane work.
5. Preserve the current endpoints and executor threads.
6. Extend shared API DTOs with active job lists while retaining compatibility fields.
7. Update CLI job watching only if it assumes global uniqueness.
8. Make dashboard enablement compatibility-aware and render multiple active jobs.
9. Update runbook/partition/degradation documentation with the egress-correct live demo.

## Questions to resolve during implementation

1. Should phase 1 allow degradation plus spam burst as well as Mine? Recommendation: no;
   land the smallest requested matrix, then add it with transaction-propagation tests.
2. Should two degradations on different nodes be allowed immediately? Recommendation:
   yes—the agent leases and qdiscs are namespace-local, and the per-node lane model
   naturally enforces conflicts.
3. Should normal config applies be allowed during degradation? Recommendation: keep them
   blocked initially to preserve simple operator expectations and loosen later.
4. What should singular `active_job_id` mean with multiple active jobs? Recommendation:
   compatibility view only, exclusive job first; all new consumers use `active_jobs`.
5. Should degradation convergence affect Mine success? Recommendation: no. Mine reports
   local RPC mining success; observation of remote arrival belongs to watchers or a
   future propagation assertion job.

## Required tests

- Degradation active, then Mine reserves and succeeds.
- Mine active, then degradation reserves and both finish independently.
- Same-node degradation plus Mine is allowed.
- Two degradations on different nodes are allowed; a second on the same node is rejected.
- Reorg, partition, scenario, faucet, spam burst, and config apply remain rejected while
  degradation is active in phase 1.
- Aborting either compatible job leaves the other active.
- Lease renewal/cleanup ownership never crosses job IDs.
- Interleaved events retain strictly increasing global sequences and correct job IDs.
- Restart with Mine plus one/two degradations recovers every lane and does not unlock an
  incompatible job early.
- Old job-store fixtures migrate singular active IDs into the right lane.
- Dashboard enables only compatible controls and renders all active jobs.
- API/MCP/CLI observe the same compatibility decisions.

Live acceptance test:

1. Start a 30-second, 5,000-ms degradation on node2.
2. Confirm phase `observing_degraded_network` and an active node2 network lease.
3. Mine one block on node2 through the dashboard or CLI.
4. Record node2's new tip time and node1/node3 arrival times with one-second watchers.
5. Confirm the remote tip is delayed by approximately the configured egress delay.
6. Confirm Mine succeeds while Degrade remains active.
7. Confirm Degrade heals at its own deadline and all agents report clear.

Implementation acceptance result: with a 5,000-ms egress delay on node2, node2 saw its
locally mined block 362 ms after submission and node1 saw it 5,411 ms after submission,
a measured propagation gap of 5,049 ms. The Mine job succeeded while the Degrade job
remained in `observing_degraded_network`. Concurrent node2 and node3 degradations were
also accepted, while a duplicate node2 degradation and a Partition were rejected. Both
leases expired independently and all three agent namespaces ended with only `noqueue`
qdiscs.

Run the repository CI-equivalent checks after implementation:

```bash
cargo ba && cargo ca && cargo fac && cargo tt
./scripts/check-compose-security.sh
./scripts/check-docker-images.sh
```
