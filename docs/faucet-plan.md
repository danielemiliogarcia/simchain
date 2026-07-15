# Implementation plan: miner-prioritized zero-fee faucet

## Status

**READY TO IMPLEMENT** — written 2026-07-15.

This is the engineering handoff for adding a user faucet to the implemented Simchain
control plane. It pins the treasury model, exact-zero-fee transaction construction,
miner-local prioritization, next-block coordination, API/CLI/MCP/dashboard contracts,
durability and recovery rules, implementation phases, and acceptance tests.

The target branch is `control-plane`. This document plans a feature; it does not claim
that the faucet is already implemented.

## 1. Decision summary

Implement the faucet as one more capability of the existing dashboard/control plane.
Do **not** add a faucet container, a second public backend, a signing service, Docker
socket access, or a repository bind mount.

The pinned behavior is:

1. A user submits one or many regtest addresses and exact satoshi amounts through the
   dashboard, HTTP API, `simchainctl`, or MCP.
2. The control plane selects one existing miner wallet (`node2` or `node3`) as the
   treasury. One transaction never combines inputs from both wallets.
3. It constructs and signs one standard transaction whose input and output values are
   equal. The actual miner fee is exactly **0 satoshis**.
4. It acquires the existing mining pause lease before selecting inputs and waits for a
   mining safe point. Spam remains running and may continue changing the mempool.
5. Before submission, it applies an absolute virtual fee delta of
   **10,000,000,000 satoshis (100 BTC)** to the transaction on **both** miner nodes with
   Bitcoin Core's `prioritisetransaction` RPC.
6. It submits the same signed raw transaction directly to node2 and node3, verifies
   zero base fee plus the expected modified fee in both mempools, persists the delivery
   record, and only then releases mining.
7. Either resident miner can therefore include the transaction in the first subsequent
   normal block. Node1 rejects the unconfirmed zero-fee transaction under ordinary
   relay policy and learns it when it validates the block.
8. The dashboard and job result label the transaction prominently as
   **SYSTEM FAUCET · 0 SAT FEE · MINER-PRIORITIZED**. The priority delta is miner-local
   policy metadata, not an on-chain payment.

The existing mining-controller coinbase destinations stay unchanged. Repeated payout
scripts do not merge rewards: every coinbase output is still an independent outpoint
with its own maturity. The faucet spends mature, confirmed UTXOs already managed by
the two miner wallets and uses a fresh wallet change script for each faucet transaction.

The parked fee-market simulation remains a separate feature. A multi-recipient faucet
will later be a useful primitive for creating independent funded actors, but this work
does not unpark or couple itself to that design.

## 2. Why the existing miner wallets remain the treasury

### Coinbase payout scripts do not need rotation

The mining controller obtains one wallet address for node2 and one for node3 and mines
multiple rewards to those scripts. This is acceptable for the simulation:

- each block still creates a distinct coinbase transaction and outpoint;
- coinbase maturity is enforced per outpoint, regardless of script reuse;
- wallet coin selection can spend any mature outpoint independently;
- the chain visibly has two miner actors, and the faucet result identifies which actor
  funded each request;
- using a new payout script per block would add address variety but would not improve
  spendability, maturity, fee selection, or next-block inclusion.

Do not modify `crates/mining-controller/src/bootstrap.rs` or the continuous mining
address strategy as part of this feature.

### Do not externalize miner keys

Sending coinbases to externally managed scripts and later constructing every treasury
spend outside Bitcoin Core would add key storage, UTXO indexing, signing, and recovery
work without improving the faucet's semantics. It would also duplicate machinery that
the miner wallets already provide safely.

The control plane may ask a miner wallet to sign a specifically constructed raw
transaction. It must never export private keys, log seed material, or become a generic
signing oracle.

### Wallet ownership does not imply mining priority

Bitcoin Core does not include a transaction merely because the local wallet owns it.
Block assembly ranks mempool transactions by mining policy. Wallet-originated traffic
still has to satisfy admission and selection rules.

The faucet therefore uses `prioritisetransaction` explicitly on both potential miners.
This RPC changes the fee value used for local mempool/mining policy; it does not change
the serialized transaction and does not transfer the virtual amount to the miner.

## 3. Goals and non-goals

### Goals

- Fund one or many caller-supplied regtest addresses in one transaction.
- Use exact integer satoshi accounting end to end.
- Make the on-chain transaction pay exactly zero satoshis in fees.
- Make the zero-fee behavior obvious in the UI, API result, logs, and explorer link.
- Arm both resident miners so alternating and weighted miner selection are both safe.
- Keep ordinary node relay/mempool policy unchanged and preserve node1 as the
  production-like observer.
- Ensure ordinary spam/mempool growth between arming and mining cannot displace the
  faucet transaction.
- Reuse the current single-active-job coordinator and mining pause lease.
- Make request retries and restart recovery at-most-once: one accepted request must
  never accidentally produce two payments.
- Preserve a configurable reserve in each miner wallet so the faucet cannot starve the
  spammer.
- Provide equivalent HTTP, CLI, MCP, and dashboard behavior over one service method.

### Non-goals

- No mainnet, signet, or testnet faucet. Regtest address validation is mandatory.
- No public arbitrary-wallet, arbitrary-input, raw-transaction, signing, or
  `prioritisetransaction` API.
- No changes to consensus, `minrelaytxfee`, `blockmintxfee`, mempool capacity, block
  capacity, or mainnet-like relay policy.
- No automatic creation or import of user wallets.
- No user-address balance index; the local mempool.space/txindex views remain the
  explorer.
- No faucet-funded spam topology or fee-market mode in this change.
- No rotating coinbase address project.
- No on-chain OP_RETURN marker in v1. The zero fee is visible on chain; Simchain's
  durable faucet metadata supplies the stronger label.
- No promise against an operator bypassing the control plane and directly mining a
  hand-selected block, submitting a conflict, or applying a larger external priority
  delta.
- No automatic re-funding after a later, explicit reorg removes a previously confirmed
  payment. That is normal Bitcoin finality behavior; submit a new idempotent request if
  a replacement payment is desired.

## 4. Target architecture

```text
Browser / simchainctl / MCP / HTTP client
                    |
                    v
       existing simchain-control-plane
       +-------------------------------------------+
       | faucet service + validation               |
       | existing job coordinator + event store    |
       | faucet transfer store + delivery guard    |
       +-----------+-------------------+-----------+
                   |                   |
           mining pause lease       Bitcoin RPC
                   |                   |
        mining-controller      +-------+-------+
                               |               |
                         node2 wallet     node3 wallet
                         priority+mempool priority+mempool
                               \               /
                                first next block
                                      |
                                      v
                            node1 validates/indexes
```

Ownership remains:

```text
Bitcoin Core wallets       keys, mature treasury UTXOs, signatures
Bitcoin Core miner nodes   local priority deltas, mempool admission, block assembly
Mining controller          safe-point pause lease and normal block scheduling
Control plane              request validation, coordination, durability, transports
Clients                    destination and amount intent
```

The faucet is a domain module inside `simchain-control-plane`. A backend trait isolates
Bitcoin RPC for tests; that trait is not a second running backend or network service.

## 5. Exact fee and priority semantics

### Actual fee versus virtual fee delta

| Value | Pinned value | Meaning |
| --- | ---: | --- |
| Actual transaction fee | `0 sat` | `sum(inputs) - sum(outputs)`; visible on chain and paid to nobody. |
| Miner priority delta | `10,000,000,000 sat` (100 BTC) | Local, virtual absolute fee added by each miner for admission and block selection. |
| Maximum faucet vsize | `10,000 vB` | Hard transaction-size guard. |
| Minimum faucet modified feerate | `1,000,000 sat/vB` | 100 BTC divided by the maximum allowed vsize. |
| Maximum simultaneously armed faucet txs | `1` | Prevents equal-priority faucet transactions from competing with each other. |

The 100 BTC delta is a code-owned safety constant:

```rust
const FAUCET_PRIORITY_DELTA_SATS: i64 = 10_000_000_000;
const FAUCET_MAX_TX_VBYTES: u64 = 10_000;
const FAUCET_PRIORITY_DOMINANCE_FACTOR: u64 = 100;
```

It is deliberately **not** an environment or runtime setting. A general priority knob
would expose miner policy mutation and make the delivery promise configuration
dependent.

At the maximum size, the virtual feerate is at least 1,000,000 sat/vB. For comparison,
the default Simchain `FALLBACK_FEE=0.0001 BTC/kvB` is 10 sat/vB and the raw spammer's
ordinary refill multiplier is 2x. The default margin is therefore tens of thousands of
times above Simchain traffic.

### Admission-time dominance check

The fixed constant is backed by a checked invariant. Before submission, query both
miner mempools and the live effective spam policy. Excluding the faucet tx itself,
calculate the greatest observed modified chunk feerate and the greatest feerate the
current spam policy can create. Require:

```text
faucet_modified_feerate
    >= 100 * max(observed_competing_chunk_feerate,
                 live_spammer_max_feerate,
                 miner_minimums)
```

Do not duplicate the spammer's fee arithmetic in the control plane. Move/expose a
shared conservative `SpamTuning::max_generated_feerate_sat_vb()` calculation in
`simchain-common` and make the spammer use the same fee-shaping constants. It must
cover the floor rate, the `+1 sat/vB` bulk premium, 2x fan-out/refill pricing, and the
optional 2x RBF replacement. With today's rules a conservative raw-engine bound is
`2 * (fallback_sat_vb + 2)`, where the second added sat/vB covers per-transaction
rounding; wallet mode is bounded by its explicit wallet pay rate plus the same rounding
allowance.

If either miner cannot prove this condition, reject before release with
`faucet_priority_invariant_failed`; do not silently weaken the next-block claim. This
also catches an intentionally extreme `FALLBACK_FEE` or an externally inserted
high-fee/priority transaction.

The spammer remains live after this check. Its effective policy is bounded by the value
used in the check, so continued ordinary spam can change mempool contents without
approaching the faucet band. A simultaneous operator change to fee policy is blocked
while a faucet transfer is armed but unconfirmed.

### What “next-block delivery” guarantees

The guarantee applies when all of these supported Simchain invariants hold:

1. node2 and node3 are the only block producers;
2. both miners and node1 agree on the tip before arming;
3. the mining controller acknowledges the pause lease at a safe point, with no
   `generate*` call in flight;
4. the faucet transaction is standard, final, conflict-free, no larger than 10,000 vB,
   and has no unconfirmed ancestors;
5. both miner mempools contain the exact txid with zero base fee and the pinned delta;
6. no other faucet transaction is awaiting its first block;
7. no caller bypasses the control plane with direct mining, a conflicting spend, or a
   larger local priority mutation;
8. no explicit control-plane job that deliberately excludes or rewrites mempool
   contents starts before delivery;
9. both miner processes remain available from final arming through the first block. A
   miner restart suspends the first-block guarantee until the delivery guard has
   reacquired a mining lease and re-verified both miners.

Under those conditions, the first normal block produced after the faucet releases its
mining lease includes the transaction, whether node2 or node3 is selected.

This is an operational guarantee inside Simchain, not a Bitcoin consensus guarantee.
No finite fee delta can compel an arbitrary external miner or make an invalid or
conflicting transaction valid.

### Miner-only unconfirmed visibility

The control plane submits directly to both miners after installing the local deltas.
Bitcoin Core may announce the transaction to peers, so this is not a cryptographic
private-broadcast protocol. Node1 has no priority delta and should reject the zero-fee
transaction under its ordinary min-relay policy. The expected state is:

```text
before the block: node2 mempool = present, node3 mempool = present, node1 = absent
after the block:  all nodes validate/index the confirmed zero-fee transaction
```

The UI should call this **miner-direct** or **miner-only unconfirmed**, not claim that
the transaction was hidden from the P2P network.

## 6. Faucet limits and treasury selection

### Boot-only configuration

| Setting | Default | Purpose |
| --- | ---: | --- |
| `FAUCET_WALLET_RESERVE_BTC` | `600` | Minimum confirmed spendable value retained in the selected miner wallet after a faucet transaction. |
| `FAUCET_MAX_REQUEST_BTC` | `100` | Maximum total recipient value in one request. |
| `NODE2_WALLET_NAME` | `node2` | Existing node2 treasury wallet name, passed to the control plane. |
| `NODE3_WALLET_NAME` | `node3` | Existing node3 treasury wallet name, passed to the control plane. |

Parse BTC settings as exact decimal amounts with at most eight fractional digits and
store them as satoshis. Reject negative, non-finite, over-precision, and out-of-range
values. These are boot-only infrastructure limits, not entries in the live tuning
catalog.

Hard code `FAUCET_MAX_OUTPUTS=100`, `FAUCET_MAX_TX_VBYTES=10_000`, and the 100 BTC
priority delta. They are safety invariants, not operator tuning.

The 600 BTC reserve is per wallet. The raw spammer may pull up to 500 BTC for branch
funding and 50 BTC for its floor pool, so the default leaves one refill budget plus
headroom. The reserve is checked against eligible confirmed UTXOs, not merely a
possibly optimistic wallet balance field. While the faucet transaction is unconfirmed,
the selected inputs and its change are not counted toward the reserve: the sum of
**unselected** eligible confirmed UTXOs must still meet the reserve.

### Request source

The request has this enum:

```text
auto | node2 | node3
```

- `auto` is the default. Select the wallet with the greatest eligible confirmed value
  above its reserve; break exact ties in favor of node2 for determinism.
- `node2` or `node3` explicitly selects an actor and fails if that wallet cannot honor
  the request while preserving its reserve.
- One faucet transaction uses one source wallet. Never combine the two actors' inputs.

Record the selected node and wallet in the job result and dashboard label.

### Eligible inputs

Use `listunspent` on the selected wallet and accept only UTXOs that are:

- confirmed and present on the common node2/node3/node1 tip;
- `spendable=true` and `safe=true`;
- mature if coinbase-created (Bitcoin Core's spendability result remains
  authoritative);
- not already locked by another wallet operation;
- not referenced by another prepared/pending faucet record.

Normal confirmed wallet change is also valid treasury value; requiring proof that
every selected input is a direct coinbase would add RPC/index work without changing
the miner-wallet actor semantics.

Select deterministically: largest value first, then greatest confirmations, then
lexicographic `txid:vout`. Prefer the fewest inputs that cover recipients plus a
non-dust change output while leaving the configured wallet reserve.

Persist the selected outpoints and an `inputs_selected` phase before calling
`lockunspent false`, then persist `inputs_locked` after Core acknowledges the lock.
This phase-before-effect barrier lets recovery undo a lock even if the control plane
crashes immediately after the RPC. The lock prevents the live spammer's wallet funding
path or another wallet call from selecting those inputs. A control-plane restart does
not lose node-side wallet locks; a bitcoind restart may, so recovery must revalidate
every input before rebroadcasting the saved raw transaction.

## 7. Transaction construction

### Canonical request

Normalize before fingerprinting or touching a wallet:

- require 1–100 outputs;
- validate every destination with `require_regtest_address`;
- require `amount_sats > 0` and total no greater than the configured maximum;
- reject duplicate addresses rather than silently merge them;
- sort canonical outputs by encoded address for stable fingerprints and output order;
- normalize source to `auto`, `node2`, or `node3`;
- require a non-empty `Idempotency-Key` no longer than the existing 200-byte limit.

Address-specific dust/standardness is checked by Bitcoin Core on both miners after the
raw transaction is prepared and prioritized. Return a destination-specific validation
error if an output is dust or nonstandard.

### Exact-zero algorithm

1. Sum selected input values as checked `u64` satoshis.
2. Sum recipient values as checked `u64` satoshis.
3. Obtain one fresh internal change address from the selected wallet with
   `getrawchangeaddress`.
4. Set `change_sats = input_sats - recipient_sats`.
5. If change is dust, choose an additional/different input; never convert change into a
   fee.
6. Build a version-2, final, non-RBF transaction locally with rust-bitcoin types,
   `Amount::from_sat`, and address scripts, then consensus-serialize it. Do not pass
   output values through JSON floating-point amounts.
7. Sign with `signrawtransactionwithwallet` on the source wallet and require
   `complete=true` with no signing errors.
8. Decode the signed transaction and independently verify:
   - expected inputs and destination scripts;
   - one expected change script when change is nonzero;
   - `sum(inputs) == sum(outputs)`;
   - `actual_fee_sats == 0`;
   - `vsize <= 10,000`;
   - final sequences and no unconfirmed ancestors.
9. Persist the signed hex, txid, selected outpoints, exact values, source, and phase
   before the first `prioritisetransaction` or broadcast call.

Do not use `sendtoaddress`, `sendmany`, `fundrawtransaction`, or
`walletcreatefundedpsbt` for final construction. Those wallet funding paths normally
insert a positive fee and may broadcast or mutate wallet state before the priority and
durability barriers are in place.

The public API and logs do not return the signed raw hex. It is private recovery data
inside the mode-0600 control state. The txid and all on-chain fields are public once
mined.

### On-chain highlighting

The transaction's exact 0-sat fee is the on-chain signal. The local priority delta is
not serialized into the transaction or block and cannot be proven from chain data
alone. Simchain adds the stronger attribution through its persisted transfer record,
job events, dashboard badge, and explorer link.

Do not add an OP_RETURN marker in v1. It would consume block space and alter the payment
shape without making miner-local policy cryptographically provable.

## 8. Job execution and mining coordination

Add `JobKind::Faucet`. It participates in the existing one-active-mutation rule while
it is preparing and arming the transaction.

### Ordered execution

The executor must follow this order:

1. **Validate request** — normalize addresses/amounts/source/idempotency; no RPC
   mutation.
2. **Preflight** — require bootstrap height at least 204, loaded source wallets, all
   three nodes on one tip, both miner RPCs reachable, mining worker reachable, and no
   unconfirmed faucet delivery.
3. **Acquire mining lease** — use the existing job-owned pause lease and start its TTL
   renewer. Wait until the mining controller reports a safe point and no generate call
   is in flight.
4. **Select and lock inputs** — keep spam running; select confirmed UTXOs, persist the
   exact selection, lock them in the source wallet, and persist the acknowledged lock.
5. **Construct and sign** — build the exact-zero transaction and run the independent
   value/size checks.
6. **Persist prepared state** — fsync the job store with raw hex, txid, inputs, values,
   source, expected outputs, and desired delta.
7. **Set node2 priority** — query the current delta and apply only the difference needed
   to reach exactly 100 BTC.
8. **Set node3 priority** — use the same idempotent set operation.
9. **Test admission** — run `testmempoolaccept` on both miners. Both must report allowed
   after their local deltas exist.
10. **Submit node2** — `sendrawtransaction` the saved hex. Already-in-mempool or
    already-in-chain is success after txid verification.
11. **Submit node3** — same raw hex and rules. It may already be present through relay;
    that is success.
12. **Verify both miners** — require the tx in both mempools with:
    - base fee `0 sat`;
    - fee delta `10,000,000,000 sat`;
    - modified fee `10,000,000,000 sat`;
    - expected vsize/weight and no ancestors;
    - the dominance invariant from section 5.
13. **Verify observer absence** — node1 should not contain the unconfirmed tx. A
    surprising node1 acceptance is a warning plus diagnostic field, not a reason to
    risk duplicate construction.
14. **Persist armed transfer** — atomically write the durable faucet transfer record
    before mining can resume.
15. **Release mining lease** — stop the renewer and release with
    `chain_changed=false`. The worker restores the user's pre-existing desired mining
    state. If mining was manually paused, it remains paused.
16. **Succeed job** — return `armed_for_next_block`. Confirmation is delivery state,
    not part of the single-active job lifetime.

Spam is deliberately not paused. The large, verified priority band is meant to survive
ordinary mempool churn while the next block is scheduled.

### Why the job ends at “armed”

The control plane supports a manually paused mining state. Keeping the faucet job
active until confirmation would block the existing manual-mine job that may be needed
to create that first block. Therefore:

- the mutation job succeeds after both miners are proven armed and mining ownership is
  safely released;
- a small durable transfer record independently moves from `armed` to `confirmed`;
- the dashboard clearly distinguishes “request succeeded/armed” from “confirmed”;
- manual `mine`, mining resume, and normal scheduled mining can create the delivery
  block.

### Pending-delivery interlock

Only one transfer may be `armed` at a time. While it awaits its first confirmation:

- allow normal mining pause/resume;
- allow a manual `mine` job on node2 or node3 only after a fresh synchronous check
  proves both miners are still armed; otherwise the delivery recovery path claims the
  coordinator first;
- allow spam to run and allow a spam burst;
- reject another faucet request with `faucet_delivery_pending`;
- reject reorg, partition, and scenario jobs that could rewrite or intentionally
  hand-select the next block;
- reject a live `FALLBACK_FEE` change that would invalidate the checked dominance
  bound;
- keep read-only status and dashboard operations available.

The interval should normally last one block (10–20 seconds with default timing). The
interlock makes the next-block statement enforceable without holding the public job
slot indefinitely.

### Partial miner failure

Preflight both miners before constructing. If a failure still occurs after either
miner accepts the transaction:

- retain and renew the mining lease;
- never rebuild with new inputs and never create a second payment;
- retry the missing idempotent priority/submission/verification step using the saved
  raw hex;
- expose progress events and permit cooperative abort;
- release mining only after both miners are armed, the transaction is already
  confirmed, or abort cleanup has made the limitations explicit.

There is no Bitcoin Core RPC that safely retracts an arbitrary accepted mempool
transaction. An abort after submission can remove Simchain's virtual deltas but cannot
promise that the transaction disappears. The result must say
`aborted_after_submission` and include the txid.

## 9. Durable state, idempotency, and recovery

### Required state

Extend the private `StoredJob` with an optional context that is not flattened into the
public `JobDetail`:

```rust
struct FaucetRecoveryContext {
    phase: FaucetPhase,
    normalized_request: FaucetJobRequest,
    source: Option<FaucetSourceNode>,
    wallet_name: Option<String>,
    selected_inputs: Vec<FaucetInput>,
    input_sats: Option<u64>,
    change_sats: Option<u64>,
    raw_tx_hex: Option<String>,
    txid: Option<String>,
    desired_priority_delta_sats: i64,
    node2_prioritized: bool,
    node3_prioritized: bool,
    node2_submitted: bool,
    node3_submitted: bool,
}
```

Effect booleans are diagnostics, not the source of truth. Recovery always queries Core
because a crash can occur after an RPC succeeds but before the next fsync.

Add a versioned atomic file under the existing narrow state directory:

```text
.simchain-control/faucet-transfers.json
```

It retains a bounded history (100 terminal records) and the one possible pending
transfer, which must never be evicted by history pruning. A pending record contains
normalized recipients, source, txid, signed raw hex for recovery, selected inputs,
zero fee, desired deltas, armed height/hash/time, and delivery status. Keep the file
mode 0600 and use the existing atomic storage/ownership helpers.

Before arming, the job context is the authority. Once the armed transfer fsync
succeeds, the transfer store becomes the recovery authority and the terminal job save
clears its duplicate raw hex. After confirmation or a definitive non-recoverable
delivery failure, clear raw hex and input-lock recovery fields from the transfer record;
retain txid, recipients, source, fee/delta facts, and confirmation/failure metadata.
Node1 txindex supplies the public transaction thereafter.

The HTTP representation omits `raw_tx_hex` and internal input-lock details.

### Job-store schema migration

Bump `JOB_SCHEMA_VERSION` from 1 to 2 and implement an explicit v1-to-v2 migration:

- deserialize v1 into its old shape;
- map every job with `faucet_recovery=None`;
- preserve IDs, events, idempotency keys, results, and active-job state exactly;
- atomically write v2 only after the complete conversion succeeds;
- reject unknown future versions;
- test migration and rollback/error behavior with fixtures.

Do not merely let an older binary ignore and later erase faucet recovery fields.

### Idempotency contract

Unlike older optional job idempotency, the faucet route requires
`Idempotency-Key` because it transfers value.

- Normalize and fingerprint the complete canonical request, including source.
- Reusing the key with the same fingerprint returns the original job and
  `reused=true`, regardless of terminal state.
- Reusing the key with a different fingerprint returns `400 validation_failed`.
- A client timeout/disconnect never cancels the server job.
- Retrying a prepared job always uses the persisted raw transaction and txid.
- Never rerun coin selection after raw hex exists.

The dashboard creates a UUID with `crypto.randomUUID()` and retains it in
`sessionStorage` until it receives a definitive response. `simchainctl` creates and
prints a UUID v4 when `--idempotency-key` is omitted. MCP requires the key explicitly
so an agent can safely retry.

### Additive priority RPC

`prioritisetransaction` is additive. Never call it blindly on retry. Implement one
helper used for arming and cleanup:

```text
set_priority(txid, desired):
    current = getprioritisedtransactions[txid].fee_delta or 0
    difference = desired - current
    if difference != 0:
        prioritisetransaction(txid, fee_delta=difference)
    verify resulting fee_delta == desired
```

The control plane owns the priority entry for faucet txids exclusively. Cleanup calls
the same helper with desired `0`. Core removes a priority entry automatically when the
transaction is mined; absence after confirmation is success.

### Delivery guard

A lightweight faucet delivery guard runs inside the control plane; it is not a new
service. Its normal polling path is read-only; any repair first claims the existing
mutation coordinator and mining lease. It polls the pending record through
node1/node2/node3 RPC and:

- marks `confirmed` when node1 txindex reports confirmations and records block hash and
  height;
- releases the pending-delivery interlock after confirmation;
- leaves an armed transaction alone while both miner mempools still prove the expected
  delta;
- if a miner restart loses the transaction/delta before confirmation, atomically claims
  the same mutation coordinator as restart recovery, acquires a mining lease, and
  re-arms the **same** raw transaction on both miners before release. Status says the
  next-block guarantee is suspended until that verification completes;
- marks `delivery_failed` if saved inputs were spent by a different transaction and
  records the conflict instead of constructing a replacement;
- exposes a stale/error field without erasing the last known state when RPC is
  temporarily unavailable.

Public mutation admission and the delivery guard must share one lock/state machine so
they cannot both mutate miner policy concurrently. Status reports the internal recovery
owner just as it reports interrupted-job lease recovery.

Once a transfer is confirmed, later explicit reorgs follow normal Bitcoin semantics.
The history record may show that its confirmation was orphaned, but v1 does not
automatically issue another payment.

### Restart recovery matrix

| Durable phase / observed state | Recovery action |
| --- | --- |
| No prepared raw transaction | Mark interrupted, unlock any recorded inputs, release owned lease. No payment could exist. |
| Raw persisted; no node has tx; inputs unspent | Reacquire/renew mining lease, restore input locks, idempotently continue priority and submission with the same raw tx. |
| Raw persisted; input spent by another tx | Clear owned deltas, mark `prepared_inputs_conflicted`, release lease; never rebuild automatically. |
| One miner has tx | Keep/reacquire mining lease, set exact priority on both, submit same raw tx to missing miner, verify both, then arm/release. |
| Both miners have tx with correct delta | Persist/repair the transfer record, release lease, mark job succeeded/armed. |
| Tx already confirmed | Persist confirmation, treat missing priority entries as expected, unlock stale locks, release lease, mark job succeeded. |
| Transfer was armed but miner restart lost mempool state | Delivery guard acquires coordinator + mining lease and re-arms saved raw tx before allowing another incompatible mutation. |
| RPC unavailable during safety-critical recovery | Remain visibly recovering, renew any acquired lease, retry with bounded backoff, and block incompatible mutations. |

Faucet recovery is intentionally resumable because every side effect is idempotently
observable and the exact signed transaction is durable. This is narrower than trying to
resume an arbitrary scenario.

### Abort semantics

| Abort point | Required result |
| --- | --- |
| Before input lock | `aborted`; no cleanup beyond lease release. |
| After input lock, before priority | Unlock inputs, release lease, `aborted`. |
| After priority, before any submission | Set both deltas to zero, unlock inputs, release lease, `aborted`. |
| After any miner accepts the tx | Set owned deltas to zero where safe, release lease, persist `aborted_after_submission` with txid and explicit “may still confirm” warning. Do not claim cancellation. |
| Already confirmed | Abort is a no-op conflict; return the confirmed state. |

## 10. Public HTTP API

All mutations keep the existing bearer-token and Host checks. Read routes follow the
current loopback-only read policy.

### Create faucet job

```http
POST /api/v1/jobs/faucet
Authorization: Bearer <token>
Idempotency-Key: 4eac4a83-3da4-4fcb-8c43-7f1bd0acfe6e
Content-Type: application/json
```

```json
{
  "source": "auto",
  "outputs": [
    {
      "address": "bcrt1q...",
      "amount_sats": 100000000
    },
    {
      "address": "bcrt1p...",
      "amount_sats": 25000000
    }
  ]
}
```

Success uses the existing job contract:

```http
HTTP/1.1 202 Accepted
```

```json
{
  "job_id": "faucet-...",
  "state": "starting",
  "reused": false
}
```

The terminal `JobDetail.result` is:

```json
{
  "delivery_state": "armed",
  "txid": "...",
  "source": "node2",
  "wallet_name": "node2",
  "outputs": [
    {"address": "bcrt1q...", "amount_sats": 100000000},
    {"address": "bcrt1p...", "amount_sats": 25000000}
  ],
  "total_sats": 125000000,
  "change_sats": 4875000000,
  "actual_fee_sats": 0,
  "priority_delta_sats": 10000000000,
  "vsize": 241,
  "armed_nodes": ["node2", "node3"],
  "visibility": "miner_only_unconfirmed",
  "armed_at_height": 412,
  "armed_at_block_hash": "...",
  "transfer_url": "/api/v1/faucet/transfers/...",
  "explorer_url": "http://127.0.0.1:1080/tx/..."
}
```

The example values are illustrative except the zero actual fee and 100 BTC priority
delta, which are invariants.

### Faucet state and transfer reads

```text
GET /api/v1/faucet
GET /api/v1/faucet/transfers/{txid}
```

`GET /api/v1/faucet` returns:

- availability and last probe error;
- configured max total, max outputs, reserve, max vsize, and pinned delta;
- confirmed eligible balance and post-reserve available balance for node2/node3;
- the pending transfer, if any;
- the most recent bounded transfer summaries.

Transfer delivery states are:

```text
armed
confirmed
recovering
delivery_failed
aborted_after_submission
orphaned_after_confirmation
```

### Errors

Extend the closed error code enum where a generic existing code is insufficient:

| HTTP | Code | Meaning |
| ---: | --- | --- |
| 400 | `validation_failed` | Bad/missing idempotency key, address, amount, duplicate output, source, dust, or cap. |
| 409 | `operation_in_progress` | Another public mutation or internal recovery owns the coordinator. |
| 409 | `faucet_delivery_pending` | One transfer is already armed and unconfirmed. |
| 409 | `insufficient_faucet_funds` | No selected wallet can fund outputs and valid change while preserving reserve. |
| 409 | `prepared_inputs_conflicted` | Durable transaction inputs were spent elsewhere; no second payment was made. |
| 503 | `faucet_unavailable` | Wallet/miner/mining worker/tip preflight failed before mutation. |
| 503 | `faucet_priority_invariant_failed` | Both miners could not prove the pinned priority/admission/selection conditions. |

Never include raw hex, RPC credentials, wallet descriptors, or internal tokens in the
error envelope.

## 11. `simchainctl` contract

Add:

```text
simchainctl faucet \
  --to bcrt1q...=1btc \
  --to bcrt1p...=25000000sat \
  [--source auto|node2|node3] \
  [--idempotency-key <uuid>] \
  [--wait] [--timeout <seconds>] [--json]

simchainctl faucet status [--json]
simchainctl faucet transfer <txid> [--watch] [--json]
```

Require an explicit `btc` or `sat` suffix in `--to` values. Parse BTC as a decimal
string with at most eight places; never pass through `f32`/`f64`. Repeated `--to`
arguments become one multi-output request.

If the caller omits `--idempotency-key`, generate UUID v4 before the request and print
it in human and JSON output so the caller can retry. `--wait` waits for the faucet job
to become terminal (armed), not indefinitely for a block. `faucet transfer --watch`
handles confirmation waiting.

Exit codes follow existing job semantics:

- `0`: armed or confirmed as requested;
- validation/client error: existing usage code;
- server job failure/abort/interruption: existing job-failure code;
- timeout while the server job remains active: existing timeout code and print the job
  ID plus idempotency key.

The CLI remains an HTTP client. It never connects to Bitcoin RPC or Docker.

## 12. MCP contract

Add one mutation tool and two read tools over the same service methods:

```text
fund_addresses(outputs, source, idempotency_key)
get_faucet_status()
get_faucet_transfer(txid)
```

`outputs` uses `{address, amount_sats}` objects. `idempotency_key` is required for the
mutation tool. The tool returns the normal `JobCreatedResponse`; agents use existing
`get_job`/events to wait for arming and `get_faucet_transfer` for confirmation.

Do not expose a generic “prioritize transaction,” “sign transaction,” or “broadcast as
miner” MCP tool.

## 13. Dashboard design

Add a Faucet action card to the existing control-plane dashboard, not a separate page
or application.

### Request form

- One destination row by default with add/remove controls up to 100 rows.
- Address field with regtest-only validation.
- BTC amount field parsed with a string decimal-to-satoshi function; never use binary
  floating-point arithmetic for the submitted value.
- Advanced source selector: Auto (recommended), node2, node3.
- Read-only source balances, available-after-reserve values, request cap, and pending
  delivery status.
- Confirmation copy that says exactly:

  ```text
  This creates a real regtest transaction with a 0 sat fee. Simchain submits it
  directly to both miners with a local 100 BTC virtual priority delta so their next
  block includes it. The virtual amount is not paid or transferred.
  ```

### Progress and result

Map faucet phases to plain-language progress:

```text
Pausing mining safely
Selecting mature miner funds
Building and signing exact-zero transaction
Arming node2
Arming node3
Verifying next-block priority
Mining restored — waiting for next block
Confirmed in block <height>
```

Show a prominent badge:

```text
SYSTEM FAUCET · 0 SAT FEE · MINER-PRIORITIZED
```

Display source actor, destinations, txid, actual fee, virtual delta, armed miners,
current delivery state, and explorer link. Before confirmation, do not label node1's
absence as “transaction missing”; say “armed in miner mempools; observer sees it after
the block.”

The browser generates one idempotency UUID when the user confirms, saves it in
`sessionStorage`, and reuses it for any retry until a definitive job ID/error arrives.
Disable duplicate submission while the request is in flight.

### Accessibility and safety

- Preserve keyboard navigation, labels, focus management, and existing responsive
  layout.
- Do not encode state only by color.
- Require an explicit confirmation click after showing total BTC, output count, source,
  and zero-fee behavior.
- Render all addresses/txids as text, never injected HTML.

## 14. Configuration and Compose changes

Pass these existing wallet names into `btc-simnet-control-plane`:

```yaml
- NODE2_WALLET_NAME=${NODE2_WALLET_NAME:-node2}
- NODE3_WALLET_NAME=${NODE3_WALLET_NAME:-node3}
- FAUCET_WALLET_RESERVE_BTC=${FAUCET_WALLET_RESERVE_BTC:-600}
- FAUCET_MAX_REQUEST_BTC=${FAUCET_MAX_REQUEST_BTC:-100}
```

Add typed fields to `ControlPlaneConfig`; do not read faucet settings ad hoc from deep
execution code. Startup must fail with a clear error for invalid limits or empty wallet
names.

No Compose service, network, volume, port, capability, or image target is added. The
control plane already has Bitcoin RPC connectivity and its narrow state directory.
Specifically:

- no `/var/run/docker.sock`;
- no repository root mount;
- no node datadir mount;
- no private-key file mount;
- no Docker CLI in the control-plane image;
- no change to node relay or mining-policy flags.

## 15. Internal code design and file map

### Shared contracts

| File | Planned change |
| --- | --- |
| `crates/simchain-common/src/control_api/jobs.rs` | Add `JobKind::Faucet` and faucet request/result references if job DTOs remain centralized. |
| `crates/simchain-common/src/control_api/faucet.rs` | New source/output/request/status/transfer DTOs and delivery-state enum; re-export from the module root. |
| `crates/simchain-common/src/control_api/error.rs` | Add only the faucet-specific closed error codes listed above. |
| `crates/simchain-common/src/config.rs` | Reuse exact amount, RPC URL, wallet client, and regtest-address helpers; add a shared exact BTC parser only if a second crate needs it. |
| `crates/simchain-common/src/live_tuning.rs` | Expose the conservative maximum generated spam feerate from the same shared constants the spammer consumes, preventing dominance-check drift. |

### Control plane

| File | Planned change |
| --- | --- |
| `crates/control-plane/src/faucet_job.rs` | New `FaucetBackend` trait, `RpcFaucetBackend`, coin selection, raw construction/signing, set-priority helper, admission/verification probes, and recovery context. |
| `crates/control-plane/src/faucet_store.rs` | Versioned atomic bounded transfer history/pending store using existing ownership/mode helpers. |
| `crates/control-plane/src/jobs.rs` | Add reservation/execution/abort/restart paths, phase events, pending-delivery interlock, and faucet dependency. Keep the single coordinator. |
| `crates/control-plane/src/job_store.rs` | Add tested job schema v1→v2 migration without weakening path/mode protections. |
| `crates/control-plane/src/state.rs` | Add typed wallet names and exact faucet limits to `ControlPlaneConfig`; expose faucet backend/store/guard through application state only as needed. |
| `crates/control-plane/src/main.rs` | Construct one RPC faucet backend and delivery guard; start it alongside existing status/reconcile loops. |
| `crates/control-plane/src/service.rs` | Add transport-independent create/status/transfer methods and map domain errors once. |
| `crates/control-plane/src/api.rs` | Add authenticated job route plus read routes and contract tests. |
| `crates/control-plane/src/mcp.rs` | Add the three faucet tools over service methods. |
| `crates/control-plane/src/status.rs` | Surface pending delivery and last-known confirmation/recovery status without duplicating mutation logic. |
| `crates/control-plane/src/test_support.rs` | Add deterministic mock faucet backend/store fixtures. Every existing fixture must supply the new dependency. |
| `crates/control-plane/static/index.html` | Faucet form/card and confirmation dialog markup. |
| `crates/control-plane/static/app.js` | Exact amount parser, request/idempotency handling, phase/result rendering, transfer polling. |
| `crates/control-plane/static/styles.css` | Reuse design tokens; add only faucet row, badge, and responsive form styles. |

Keep RPC code out of `api.rs`, `mcp.rs`, and the browser. Keep business validation out
of individual transports.

### CLI and deployment/docs

| File | Planned change |
| --- | --- |
| `crates/simchainctl/src/commands/mod.rs` and command module | Parse faucet subcommands and exact suffixed amounts. |
| `crates/simchainctl/src/client.rs` | Typed create/status/transfer calls and required idempotency header. |
| `crates/simchainctl/src/output.rs` | Human/JSON rendering for armed versus confirmed state. |
| `crates/simchainctl/src/main.rs` | Command wiring and wait/exit behavior. |
| `crates/simchainctl/Cargo.toml` | Add a direct UUID v4 dependency if the existing dependency graph cannot provide the API directly; update `Cargo.lock`. |
| `docker-compose.yml` | Pass wallet names and two boot-only faucet limits to the existing service. |
| `.env.example`, `.env.full.example` | Document optional limits and defaults. |
| `docs/INTRO.md`, `docs/SETTINGS.md`, `docs/RUNBOOK.md` | Describe user workflow, settings, recovery, zero-fee semantics, and diagnosis. |
| `docs/NICE-TO-HAVE.md` | At most link the parked fee-market item to this independent primitive; do not mark fee-market work implemented. |

Do not create a `crates/faucet`, `faucet` Compose service, or another web server.

## 16. Implementation phases and commit boundaries

Each phase must end with a clean working tree after a `--no-gpg-sign` commit and with
the repository gates passing. Do not begin a later phase on top of a known failure.

### Phase 1 — Contracts, configuration, and RPC backend

Deliver:

1. shared faucet DTOs/enums and `JobKind::Faucet`;
2. typed faucet config with exact parsing and Compose/env propagation;
3. `FaucetBackend` plus RPC implementation for wallet discovery, confirmed UTXO
   selection, locking, exact-zero construction/signing, idempotent priority setting,
   test admission, and mempool verification;
4. shared spam-policy maximum-feerate calculation consumed by both spammer and faucet
   preflight;
5. mock backend and focused unit tests;
6. no public route or UI yet.

Suggested commit:

```text
feat(control-plane): add faucet contracts and rpc backend
```

Phase gate:

```bash
cargo ba && cargo ca && cargo fac && cargo tt
docker compose config --quiet
git diff --check
```

### Phase 2 — Durable job, delivery guard, and recovery

Deliver:

1. v1→v2 job-store migration;
2. private `FaucetRecoveryContext` and phase-before-effect persistence;
3. atomic bounded faucet transfer store;
4. faucet executor using the mining lease and the exact order in section 8;
5. pending-delivery interlock, read-only observation, and coordinator-owned delivery
   recovery;
6. automatic same-raw-tx re-arm after miner restart;
7. abort and restart matrix tests, including failures after every RPC boundary.

Suggested commit:

```text
feat(control-plane): coordinate durable faucet jobs
```

Run the full phase gate again.

### Phase 3 — HTTP, CLI, and MCP transports

Deliver:

1. `/api/v1/jobs/faucet`, `/api/v1/faucet`, and transfer read route;
2. required HTTP idempotency contract and error mapping;
3. `simchainctl faucet` create/status/transfer commands with exact amount parsing;
4. MCP mutation/read tools;
5. parity tests proving all transports call the same service behavior.

Suggested commit:

```text
feat(control-plane): expose faucet api cli and mcp
```

Run the full phase gate again.

### Phase 4 — Dashboard and operator documentation

Deliver:

1. multi-destination faucet form, confirmation, progress, result, and transfer polling;
2. prominent zero-fee/miner-priority labels and explorer link;
3. responsive/accessibility tests already used by the static UI suite;
4. settings, introduction, and runbook updates;
5. no change to the parked fee-market feature status.

Suggested commit:

```text
feat(dashboard): add miner-prioritized faucet workflow
```

Run the full phase gate again.

### Phase 5 — Live Bitcoin Core integration and security acceptance

Deliver:

1. a repeatable live-stack test against the pinned Bitcoin Core 31.1 image;
2. node2-next and node3-next variants;
3. saturated/changing mempool verification;
4. desired-mining-paused/manual-mine verification;
5. restart recovery and partial miner failure drills;
6. no-socket/no-extra-service image and Compose inspection;
7. final docs corrections discovered by the live run.

Suggested commit:

```text
test(control-plane): cover live zero-fee faucet delivery
```

Final gate:

```bash
cargo ba && cargo ca && cargo fac && cargo tt
docker compose config --quiet
git diff --check
```

Also build the affected Docker targets with the committed lock file and run the
repository's existing security/no-Docker-socket checks. Every implementation commit
uses `git commit --no-gpg-sign` as requested.

## 17. Automated verification plan

### Contract and validation tests

- Empty output list, 101 outputs, zero amount, overflow, over-cap total, duplicate
  addresses, non-regtest address, invalid source, and dust destination all fail without
  wallet or miner mutation.
- Canonically equivalent requests produce one fingerprint.
- Same idempotency key/request returns the same job; a different request is rejected.
- BTC parsing accepts exactly eight decimals and rejects floats/exponents/extra
  precision.
- Auto source selection and deterministic tie-breaking are stable.
- Explicit source preserves the per-wallet reserve or fails.

### Transaction tests

- Selected inputs are confirmed, safe, spendable, unlocked, and from one wallet.
- Recipients plus change equal inputs exactly; actual fee is zero.
- Dust change causes reselection rather than a hidden fee.
- Signed transaction is complete, final, non-RBF, standard, and at most 10,000 vB.
- Raw hex/txid is persisted before any priority or broadcast effect.
- No raw hex is present in public API/job output or ordinary logs.

### Priority and coordination tests

- `set_priority` is idempotent when current delta is zero, partial, exact, excessive,
  negative, or absent.
- Both miners reach exactly 10,000,000,000 sats; retry never doubles it.
- Admission sees base fee 0 and modified fee 100 BTC.
- Dominance failure blocks release and reports the observed/configured competitor.
- Mining lease is acquired at a safe point and held through both miner verifications.
- Spam is never leased/paused by the faucet.
- Releasing the lease restores desired running or desired paused state correctly.
- A second pending faucet and incompatible jobs are rejected; manual mine is allowed.

### Failure/restart tests

Inject a crash/error immediately before and after each of:

```text
input lock
prepared-state fsync
node2 priority
node3 priority
node2 submission
node3 submission
armed-transfer fsync
mining-lease release
job terminal fsync
```

For every boundary prove:

- at most one txid/payment exists;
- recovery uses the same raw hex;
- additive deltas finish at exactly the target;
- mining is not released while only one miner is armed;
- confirmed state is recognized without rebroadcast;
- conflicting inputs fail explicitly without new coin selection;
- abort before submission removes deltas and unlocks inputs;
- abort after submission says the tx may still confirm.

Test job-store v1→v2 migration, faucet-store truncation/history bounds, mode 0600,
atomic-write failure, corrupt state refusal, and unknown future schema rejection.

### API/CLI/MCP/dashboard tests

- Mutation auth/Host checks and required idempotency header.
- Exact request/response/error envelope and `202` semantics.
- CLI suffix parsing, UUID generation, JSON stability, waits, timeouts, and exit codes.
- MCP schemas and service parity; no generic signing/priority tool appears.
- Dashboard produces integer sats, retains the retry UUID, prevents double submit,
  renders every phase/error, and distinguishes armed from confirmed.

### Live Core 31.1 acceptance test

1. Start the ordinary stack and wait for height at least 204.
2. Keep spam running and pause mining through the control plane.
3. Submit one faucet request with at least two fresh destination address types.
4. Continue adding ordinary spam while the faucet arms.
5. Assert node1 would reject the raw zero-fee tx with min-relay policy and does not
   contain the txid.
6. Assert node2 and node3 both contain the txid with base fee 0, exact 100 BTC fee
   delta, exact modified fee, and the dominance margin.
7. Resume mining or submit one normal manual-mine job.
8. Assert the **first** subsequent block contains the faucet tx.
9. Assert node1 txindex reports the exact outputs and fee 0.
10. Assert both priority maps automatically drop the mined txid.
11. Assert the transfer becomes confirmed and the pending interlock clears.
12. Repeat once forcing node2 as next miner and once forcing node3.
13. Repeat with mining desired-paused and a manual block.
14. Pause mining, restart one miner after arming, then prove the UI suspends its
    first-block claim and the delivery guard re-arms the same tx under a mining lease
    before restoring that claim. Separately document that an uncontrolled restart may
    allow an intervening block and therefore suspends the original first-block claim.
15. Inspect Compose and the running container: no Docker socket, extra faucet service,
    repo mount, Docker CLI, or new capability exists.

## 18. Observability

Emit structured job events without raw hex:

```text
faucet_preflight_completed
mining_lease_acquired
faucet_inputs_locked
faucet_transaction_prepared
faucet_priority_set        {node, desired_delta_sats, previous_delta_sats}
faucet_submission_accepted {node, already_present}
faucet_miner_verified      {node, base_fee_sats, modified_fee_sats, vsize}
faucet_armed               {txid, source, output_count, total_sats}
faucet_confirmed           {txid, height, block_hash}
faucet_recovery_started
faucet_recovery_completed
```

Log addresses and amounts only where the existing job request already makes them
operator-visible. Never log RPC URLs with credentials, tokens, descriptors, private
keys, or signed raw hex.

Status must make these distinct:

- faucet unavailable before mutation;
- active faucet job preparing/arming;
- transfer armed and waiting because mining is intentionally paused;
- internal delivery recovery;
- confirmed;
- delivery failed or aborted after submission.

## 19. Failure behavior

| Failure | Required behavior |
| --- | --- |
| Bootstrap incomplete | Fail preflight without wallet mutation. |
| Wallet missing/locked/unreachable | Fail preflight with source-specific diagnostic. |
| Nodes disagree on tip | Fail before input lock; never construct against an ambiguous UTXO set. |
| Insufficient value after reserve | Return exact eligible/required/reserve figures, without mutation. |
| Mining lease safe-point timeout | Release any partial lease and fail before input selection. |
| Transaction nonstandard/dust/too large | Clear any installed deltas, unlock inputs, fail; never round into a fee. |
| Dominance condition not met | Keep mining paused during cleanup, remove deltas, unlock if unsubmitted, and fail explicitly. |
| One miner fails after the other accepts | Retain lease and retry same tx; do not release a one-miner guarantee. |
| Node1 unexpectedly accepts unconfirmed tx | Record warning; do not duplicate or rebuild. Zero fee and two-miner inclusion remain the primary invariants. |
| Control plane crashes before preparation | Existing lease TTL recovery; no payment exists. |
| Control plane crashes after preparation | Faucet-specific same-raw recovery matrix. |
| Miner restarts after job success, before block | Suspend the first-block claim; delivery guard re-arms the same raw tx under coordinator + lease. If a block raced before recovery, promise future delivery, not retroactive first-block inclusion. |
| Mining desired-paused | Job succeeds as armed; lease release preserves pause; UI tells user to mine/resume. |
| Client disconnects | Server job continues; retry with same idempotency key. |
| Abort after submission | Cannot promise retraction; clear virtual priority and report txid/may-confirm state. |
| Later explicit reorg orphans confirmed payment | Record orphaned history; no automatic second payment in v1. |

## 20. Documentation updates at implementation time

- `docs/INTRO.md`: add the faucet to control-plane capabilities and explain the two
  miner treasuries.
- `docs/SETTINGS.md`: document the two boot-only limits and wallet-name propagation;
  state that the 100 BTC value is virtual and not configurable.
- `docs/RUNBOOK.md`: add create, watch, manually mine while paused, insufficient funds,
  pending delivery, recovery, and abort-after-submit procedures.
- `README.md`: one concise dashboard/CLI example and zero-fee warning.
- `.env.example` and `.env.full.example`: optional limits with safe comments.
- `docs/NICE-TO-HAVE.md`: preserve “Fee-market simulation in the spammer — PARKED”; it
  may link here as a future independent-UTXO funding primitive.

Do not edit the project description to imply a public/mainnet faucet.

## 21. Acceptance criteria

The feature is complete only when all are true:

- One API request funds 1–100 valid regtest addresses through one signed transaction.
- The transaction's actual fee is provably 0 sat before submission and after mining.
- Node2 and node3 each show the pinned 100 BTC virtual delta and accept the tx.
- The verified priority band dominates current and policy-bounded ordinary Simchain
  traffic by at least 100x.
- The first normal block after arming contains the tx for either possible miner.
- Node1 does not require relaxed relay policy and indexes the tx after the block.
- The mining safe-point lease closes the arming race; spam remains live.
- Existing coinbase payout behavior is untouched.
- Per-wallet reserve and request/output/size caps are enforced.
- Same-key retries and crash recovery can produce only the original txid.
- Partial-miner and miner-restart recovery use the persisted raw tx, never a new
  payment.
- The UI/API/CLI/MCP all use one service/domain implementation and clearly distinguish
  actual fee from virtual priority.
- A manually paused network can arm, then include through the existing manual-mine
  action without deadlocking the job coordinator.
- No new backend process, Docker service, socket mount, broad filesystem mount, relay
  policy exception, or exported key exists.
- `cargo ba && cargo ca && cargo fac && cargo tt` and the live Core/Compose acceptance
  suite pass from a clean tree.

## 22. Explicit follow-on boundary: fee-market simulation

The multi-output faucet can later seed many independently controlled addresses, which
could remove long unconfirmed chains from a future fee-market spam design. That is a
useful connection but a different problem:

- the faucet manages user/test-actor treasury distribution and reliable delivery;
- the parked spammer feature manages transaction topology, keys, refill cadence, fee
  distributions, and sustained block pressure.

Do not add spammer key generation, address fan-out ownership, faucet auto-refill, or fee
market controls to this implementation. Revisit the parked plan only after the faucet
is independently shipped and measured.

## 23. Bitcoin Core references

The design relies on behavior documented and tested by the pinned Core line:

- [`prioritisetransaction` 31.0 RPC](https://bitcoincore.org/en/doc/31.0.0/rpc/mining/prioritisetransaction/)
  defines an absolute satoshi fee delta that affects block selection but is not paid.
- [`getprioritisedtransactions` 31.0 RPC](https://bitcoincore.org/en/doc/31.0.0/rpc/mining/getprioritisedtransactions/)
  exposes the current delta, mempool presence, and modified fee needed for idempotent
  verification.
- [Bitcoin Core v31.1 `mining_prioritisetransaction.py`](https://github.com/bitcoin/bitcoin/blob/v31.1/test/functional/mining_prioritisetransaction.py)
  tests a large 86 BTC delta, additive behavior, automatic removal after mining, and
  admission of an otherwise rejected zero-fee transaction after prioritization.
- [`sendrawtransaction` 31.0 RPC](https://bitcoincore.org/en/doc/31.0.0/rpc/rawtransactions/sendrawtransaction/)
  documents raw submission and its peer-announcement behavior.
- [`signrawtransactionwithwallet` 31.0 RPC](https://bitcoincore.org/en/doc/31.0.0/rpc/wallet/signrawtransactionwithwallet/)
  signs the locally constructed integer-valued transaction without funding or
  broadcasting it.
- [`testmempoolaccept` 31.0 RPC](https://bitcoincore.org/en/doc/31.0.0/rpc/rawtransactions/testmempoolaccept/)
  supplies the non-mutating standardness/admission check used after local priority is
  installed.

Re-run the live acceptance test whenever the default Bitcoin Core image changes; do not
assume miner-policy RPC behavior across an untested major upgrade.

## 24. Final design rule

The faucet is a deliberately visible Simchain system transaction: **real outputs,
exactly zero real fee, an extremely high but purely virtual miner-local priority, and
one durable control-plane workflow**. Reliability comes from arming both miners under
the existing mining lease and persisting one signed transaction before mutation—not
from weakening node policy, trusting wallet ownership, or adding infrastructure.
