# Implementation plan: raw-engine double-spends during reorgs

## Status

**READY FOR EFFORT/PRIORITY DECISION** — written 2026-07-11.

This is a handoff plan, not an implemented feature. It extends the existing
`REORG_DOUBLE_SPEND_PCT` implementation to transactions produced by the default raw
spammer (`USE_RAW_TX_SPAM=true`). The completed wallet-engine behavior and rationale
are documented in [REORGS.md](REORGS.md).

## 1. Decision summary

The enhancement is feasible.

The raw spammer already derives reproducible, regtest-only private keys from fixed tags
and miner wallet names. The reorg tool does not need the spammer to export private keys
or expose a general signing API. The safest design is:

1. move deterministic raw-key derivation and P2WPKH signing into
   `simchain-common`,
2. let both the spammer and reorg tool use that shared implementation,
3. add a small pause/resume lease protocol to the spammer,
4. have the reorg tool acquire the lease before invalidation and release it only after
   the winning chain is adopted,
5. force the spammer to reconcile its branch and floor-pool state before it resumes.

Sharing keys without coordination is not sufficient. The spammer could otherwise create
a new spend of an input while the reorg tool is constructing or spreading conflicts
across replacement blocks. That race can invalidate a planned conflict or cause a block
to be rejected.

### Recommended decision

- Implement this if permanent-drop testing must work with the default raw engine while
  the normal spam service remains live.
- Leave it as a nice-to-have if switching to `USE_RAW_TX_SPAM=false` for the specific
  double-spend scenario is acceptable. The recommended implementation is a
  medium-to-large change because the difficult part is process coordination and state
  recovery, not signing.

### Estimated effort

| Scope | Estimate | Result |
| --- | ---: | --- |
| Shared deterministic signer only | 2–4 engineer-days | Can sign raw spam conflicts, but safe only when the spammer is explicitly stopped. Not suitable for auto-reorg mode. |
| Recommended signer + live coordination | 6–10 engineer-days | Safe one-shot and auto reorgs with a running raw spammer, recovery, tests, and docs. |

Expected size for the recommended implementation: roughly 500–900 production lines and
300–500 test lines across the common, spammer, and reorg crates. Treat these as planning
ranges, not commitments.

## 2. Current behavior and exact limitation

Today `crates/reorg/src/double_spend.rs` only considers a transaction when the reorg
node's loaded wallet recognizes it and can sign a replacement. Raw spam bypasses those
wallets, so its transactions normally produce zero eligible roots. The tool now warns
when `REORG_DOUBLE_SPEND_PCT > 0` is combined with `USE_RAW_TX_SPAM=true` and eligibility
is zero.

The raw engine owns two deterministic P2WPKH identities for each miner wallet name:

| Role | Current derivation tag | Purpose |
| --- | --- | --- |
| Branch/data key | `simchain-raw-spam-{wallet_name}` | DATA/OP_RETURN chains, gap sealers, OUTPUT-mode spam, branch fan-outs, and their change. |
| Floor-pool key | `simchain-raw-floor-{wallet_name}` | Confirmed floor-fill ammo, floor fills, and floor-pool fan-outs/change. |

Each secret key is currently `sha256(tag)` interpreted as a secp256k1 secret. This is
intentionally public and deterministic because the network is throwaway regtest. A
spammer restart derives the same addresses and recovers confirmed UTXOs with
`scantxoutset`.

There are normally four relevant identities:

- node2 branch/data,
- node2 floor pool,
- node3 branch/data,
- node3 floor pool.

The reorg node must consider both miner names, not only `REORG_WALLET_NAME`. Blocks in
the orphaned range can contain traffic produced by either miner, and floor fills are
explicitly relayed between miner nodes.

## 3. Goals and non-goals

### Goals

- Make `REORG_DOUBLE_SPEND_PCT=1..100` select eligible roots from the union of:
  - reorg-wallet transactions supported today,
  - raw branch/data transactions,
  - raw floor-pool transactions.
- Preserve deterministic oldest-block/transaction selection.
- Mine same-input conflicts directly as raw hex, without requiring mempool RBF policy.
- Keep raw-engine funds recoverable by the spammer after the reorg.
- Prevent the live spammer from racing the reorg tool for selected inputs.
- Support both one-shot and `REORG_MODE=auto` operation.
- Resume spam automatically and reconcile its in-memory state after success or failure.
- Log source-specific eligible/selected counts and every old/new txid mapping.

### Non-goals

- Supporting arbitrary user-owned external keys.
- Turning the spammer into a general-purpose signing oracle.
- Exporting WIF/private keys through environment variables, files, logs, or HTTP.
- Changing Bitcoin Core relay, mempool, standardness, or block-capacity policy.
- Guaranteeing that every raw descendant is independently replaced. The existing
  root-only semantics remain: replace an ancestor and prune its descendants.
- Making these deterministic regtest keys appropriate for production use.

## 4. Architecture options considered

### Option A — reorg derives the same keys, no cooperation

Move key derivation/signing to `simchain-common` and let the reorg tool sign directly.

**Advantages**

- Smallest code change.
- No new control protocol or network service.
- No key transport; deterministic identities are reproduced locally.

**Problems**

- Unsafe with the live spammer. Both processes can spend the same branch or pool input.
- A conflict planned for a later replacement block can be invalidated by a spam round.
- Auto-reorg mode cannot safely ask Docker to pause the spammer; the reorg container has
  no Docker socket and should not receive one.
- Stopping the spammer from the host wrapper only helps one-shot runs, not continuous
  mode or direct container invocation.

**Conclusion:** acceptable only as an explicitly offline/manual mode. Do not ship this
alone as complete raw-engine support.

### Option B — spammer signs conflicts over an API

Add a service that receives original transactions and returns signed replacements.

**Advantages**

- Private key use remains inside the spammer process.
- Natural if raw keys become random or secret in the future.

**Problems**

- Requires an authenticated, narrowly constrained signer protocol.
- Couples every candidate and replacement to live spammer availability.
- Requires moving mutable `RawSpammer` ownership across the existing cycle loop or
  routing signer requests through channels.
- A general raw-transaction signing endpoint is easy to misuse and unnecessary because
  the current keys are deliberately deterministic/public.
- Pause/resume coordination is still required, so this does not remove the hard part.

**Conclusion:** not recommended for the present key model. Reconsider only if key
derivation stops being public/deterministic.

### Option C — shared signer plus cooperative pause/resume lease

Both tools use the same common derivation/signing code. The spammer exposes only a small
control service that pauses cycles, renews/releases a lease, and requests post-reorg
reconciliation. It never exposes a signing endpoint.

**Advantages**

- No duplicated cryptographic logic or key transport.
- Works in once and auto modes.
- Small control-plane attack surface: pause/status/resume only.
- Spammer explicitly knows a reorg occurred and can repair in-memory branch state.
- Exact engine/wallet-name handshake catches Compose/environment drift.

**Conclusion:** recommended.

## 5. Pin the new semantics

### Eligible raw root

An orphaned transaction is raw-engine eligible only when all of these are true after
invalidation:

1. every input prevout exists in the rolled-back confirmed UTXO set
   (`gettxout(..., include_mempool=false)`),
2. every input prevout script matches the same known raw identity,
3. that identity is one of the configured node2/node3 branch or floor identities,
4. the replacement output remains non-dust after the one-satoshi differentiation
   described below,
5. the transaction is not coinbase.

Requiring every input to match one signer handles multi-input raw fan-outs while
rejecting mixed-owner transactions. A transaction whose input is created by another
orphaned transaction remains a descendant and is ineligible, exactly as today.

### Percentage population

`REORG_DOUBLE_SPEND_PCT` applies to one deterministic ordered union. Source does not
reorder candidates:

```text
eligible roots of either source, preserving orphaned block/tx order
```

Do not calculate separate percentages per source. Preserve the current rule:

```text
0                         if pct == 0 or eligible == 0
max(1, floor(n*pct/100))  otherwise
```

The summary log must break the union down by source so the operator can understand what
was selected:

```text
Double-spend mode: selected 20 of 80 eligible roots (wallet=0, raw-branch=14, raw-floor=66)
```

### Raw replacement output

The replacement must retain ownership under the same raw identity so the spammer does
not leak its branch funds into an unrelated wallet.

For raw candidates:

- use the same inputs,
- create one output to the matched raw identity's P2WPKH script,
- set its value to `sum(original_outputs) - 1 sat`,
- therefore pay the original absolute fee plus one satoshi.

The one-satoshi difference is required. A floor fill is already a one-input/one-output
self-transfer to the floor key; recreating the same value/script could reproduce the
same transaction instead of a conflict. Subtracting one satoshi guarantees a different
output while keeping the replacement funds recoverable by the same signer.

Skip the candidate when `sum(original_outputs) - 1 sat` is dust. Wallet candidates keep
their current fresh-wallet-address behavior and original absolute fee.

### Raw identity names

Use the existing `NODE2_WALLET_NAME` and `NODE3_WALLET_NAME` values. Pass both into the
reorg service and derive both key roles for each unique name. Do not assume literal
`node2`/`node3`, and do not reuse only `REORG_WALLET_NAME`.

## 6. Shared raw identity and signer module

Create a new module in `crates/simchain-common`, for example:

```text
crates/simchain-common/src/raw_spam_keys.rs
```

Suggested public surface:

```rust
pub enum RawSpamKeyRole {
    Branch,
    Floor,
}

pub struct RawSpamIdentity {
    wallet_name: String,
    role: RawSpamKeyRole,
    address: Address,
    script_pubkey: ScriptBuf,
    // secret/public key fields remain private
}

impl RawSpamIdentity {
    pub fn derive(wallet_name: &str, role: RawSpamKeyRole) -> Self;
    pub fn address(&self) -> &Address;
    pub fn script_pubkey(&self) -> &ScriptBuf;
    pub fn sign_p2wpkh_transaction(
        &self,
        tx: &mut Transaction,
        prevout_amounts: &[Amount],
    ) -> Result<(), RawSignError>;
}
```

Requirements:

- Preserve the two existing derivation tags byte-for-byte.
- Do not implement `Debug`, `Display`, `Serialize`, or getters for the secret key.
- Validate that `prevout_amounts.len() == tx.input.len()`.
- Sign every input with `EcdsaSighashType::All` and P2WPKH witness construction, matching
  current spammer behavior.
- Keep dependency declarations local to each crate; do not add a
  `[workspace.dependencies]` table.
- Update `Cargo.lock` in the same change if dependencies move or are added.

Refactor `RawSpammer::new` and its private `signed_tx` implementation to consume this
module. This refactor must be behavior-neutral before reorg support is enabled.

### Compatibility tests

Pin the existing node2/node3 branch and floor addresses in unit tests. The test vectors
must be captured from the current implementation before refactoring. This prevents a tag
or compression change from silently making all existing snapshot funds undiscoverable.

Also sign a fixed transaction with both the old helper (temporarily in the test) and the
new common helper and assert identical serialized bytes. Remove the duplicated old
helper only after this equivalence test is established.

## 7. Spammer control protocol

Add a small internal-only control module:

```text
crates/spammer/src/control.rs
```

Recommended transport: HTTP/JSON on the existing Compose network, with no host port
published. A small synchronous server is sufficient; avoid introducing an async runtime
only for this protocol.

Suggested internal defaults:

```dotenv
SPAM_CONTROL_LISTEN=0.0.0.0:18450
REORG_SPAMMER_CONTROL_URL=http://btc-simnet-spammer:18450
REORG_SPAMMER_PAUSE_TIMEOUT_SECS=180
```

The 180-second pause timeout covers observed raw initialization/fill cycles near 100
seconds. Keep lease TTL and heartbeat intervals internal constants initially (suggested
TTL 600 seconds, heartbeat every 30 seconds) rather than adding more user settings.

### Endpoints

```text
GET    /v1/status
POST   /v1/reorg/lease
POST   /v1/reorg/lease/{lease_id}/renew
DELETE /v1/reorg/lease/{lease_id}
```

Pause request:

```json
{
  "request_id": "opaque-id",
  "ttl_seconds": 600
}
```

Successful response must not return keys:

```json
{
  "lease_id": "opaque-id",
  "state": "paused",
  "engine": "raw",
  "wallet_names": ["node2", "node3"],
  "tip_height": 250
}
```

Release request should include whether chain history changed:

```json
{
  "chain_changed": true
}
```

### Lease rules

- Only one active reorg lease is allowed.
- Repeating the same `request_id` is idempotent and returns the same lease.
- A different concurrent request receives HTTP 409.
- Pause acknowledgement means no spam cycle or batch RPC is still submitting
  transactions.
- The reorg client renews the lease while planning/mining.
- The spammer automatically expires the lease if the reorg process dies, conservatively
  reconciles as though history may have changed, and only then resumes.
- Normal release is explicit; TTL is crash recovery, not the normal path.
- No endpoint signs transactions or exposes secrets.

### Pausing the current loop

The current spammer owns both `RawSpammer` instances in its block loop and can spend
tens of seconds inside a cycle. Implement a shared control state plus cooperative pause
checks:

1. check before starting each cycle,
2. check between floor, small-tx, DATA, and RBF phases,
3. check inside long per-transaction loops,
4. finish an RPC batch already submitted, then acknowledge paused,
5. never acknowledge while a mutable branch update is half-applied.

Each successful send already updates in-memory state immediately, so returning early at
the existing loop boundaries is safe. Funding/fan-out waits must be allowed to finish or
be canceled only after their state has been reconciled; do not pause forever waiting for
a block after the reorg tool has already invalidated history. The lease must always be
acquired before invalidation.

### Explicit offline override

For advanced manual use, an optional enum is preferable to silently proceeding when the
control service is unavailable:

```dotenv
REORG_SPAMMER_COORDINATION=required  # default
# off = operator guarantees the spammer is stopped
```

With `required`, a one-shot reorg must fail before invalidation if the lease cannot be
acquired. Auto mode should log and skip that scheduled reorg, then retry at the next
interval. With `off`, emit a prominent warning that safety depends on the spammer being
stopped.

## 8. Reorg-side lease lifecycle

Add a client module, for example:

```text
crates/reorg/src/spammer_control.rs
```

Construct the client once in `runner.rs`; acquire a lease per reorg run only when:

```text
REORG_DOUBLE_SPEND_PCT > 0 && USE_RAW_TX_SPAM=true && !empty_mode
```

The sequence must be:

1. check that the chain is long enough,
2. acquire the spammer lease and verify `engine == "raw"`,
3. verify the returned wallet-name set matches the configured signer names,
4. wait briefly for already-sent P2P traffic to settle,
5. capture the exact orphan branch,
6. invalidate,
7. build and sign the mixed wallet/raw plan,
8. mine all replacement blocks, with raw conflicts prioritized by the existing
   weight-aware packer,
9. verify replacements and witness adoption,
10. release with `chain_changed=true`,
11. spammer reconciles and resumes.

Use an RAII-style lease guard for best-effort release on ordinary error paths, plus TTL
for process death. The guard should run a heartbeat while the lease is held. Control
errors require either a typed `ReorgError` wrapping RPC/control errors or changing the
internal reorg return type to `anyhow::Result`; do not force control errors into a fake
`bitcoincore_rpc::Error`.

Hold the lease through witness adoption. Resuming while node2/node3 disagree on the
winning branch can cause the spammer to broadcast against different histories.

## 9. Extend eligibility and signer classification

Refactor `double_spend.rs` so structural root assessment returns input values and
scripts, not values alone.

Suggested internal types:

```rust
struct ResolvedInput {
    rpc_input: CreateRawTransactionInput,
    outpoint: OutPoint,
    amount: Amount,
    script_pubkey: ScriptBuf,
}

enum ReplacementSigner {
    Wallet,
    Raw {
        wallet_name: String,
        role: RawSpamKeyRole,
    },
}

struct EligibleRoot {
    original_txid: Txid,
    inputs: Vec<ResolvedInput>,
    output_total: Amount,
    signer: ReplacementSigner,
}
```

For each orphaned non-coinbase tx, oldest-first:

1. fetch/decode the original transaction,
2. resolve every input with `gettxout(..., false)`, including value and script,
3. skip immediately if any input is absent (descendant/non-root),
4. compare all input scripts against the four configured raw identities,
5. if every input matches one identity, classify as raw,
6. otherwise apply the existing wallet recognition/signing path,
7. otherwise mark it unsupported.

Raw classification should precede wallet signing so deterministic raw transactions do
not consume wallet keypool addresses or signer RPCs. Deduplicate signer names if node2
and node3 are configured with the same wallet name.

When signing selected raw candidates:

- construct a version-2, locktime-zero transaction with the same outpoints,
- use the existing non-RBF/max sequence unless a consensus reason requires preserving
  the original sequence,
- create the one-satoshi-reduced same-owner output,
- sign locally with the matched `RawSpamIdentity`,
- compute txid/weight and add it to the existing `ReplacementTx` queue.

Wallet candidates continue through `signrawtransactionwithwallet` unchanged.

## 10. Prevent late-spend conflicts

The pause lease removes expected spammer races, but block construction should still be
defensive against already-propagating transactions or another actor spending a selected
input.

Before each replacement block that still has pending raw conflicts:

1. query the current mempool spender for every selected input outpoint (use
   `gettxspendingprevout` on supported Core versions),
2. exclude any spender other than the planned replacement,
3. add that spender's mempool descendants to the exclusion set,
4. keep raw conflicts before ordinary mempool txids in `generateblock`.

This is in addition to excluding each original txid and its known descendants. It
prevents a late alternative spend from being mined in an earlier replacement block and
invalidating a conflict scheduled later.

If `gettxspendingprevout` is unavailable, the feature should fail validation at startup
or fall back to inspecting mempool transactions explicitly; do not silently omit this
check while claiming live-spammer safety.

## 11. Post-reorg spammer reconciliation

On lease release with `chain_changed=true`, reconcile both node2 and node3 engines before
the next spam cycle.

For each branch/data engine:

- retain an in-memory branch tip only if `gettxout(tip, include_mempool=true)` still
  reports it unspent,
- rescan confirmed UTXOs for the branch address,
- add newly confirmed raw replacement outputs,
- deduplicate outpoints and reset the round-robin cursor.

For each floor pool:

- validate every `fills_inflight` outpoint against chain/mempool state,
- move confirmed surviving outputs into `pool_utxos`,
- keep still-unconfirmed valid fills in `fills_inflight`,
- drop conflicted/missing fills,
- rescan confirmed floor-key UTXOs and deduplicate.

This matters because selected raw roots and their descendants intentionally disappear.
Without reconciliation, the spammer will repeatedly attempt missing/spent branch tips,
miscount standing floor fills, or unnecessarily pull new wallet funds.

Log one resume summary per engine:

```text
Node 3 => reorg reconciliation: 31 branch tips retained, 8 replacement outputs recovered, 74 stale tips dropped
Node 3 => floor reconciliation: 1900 standing, 420 confirmed ammo, 100 stale fills dropped
```

## 12. File-by-file implementation map

| File | Required changes |
| --- | --- |
| `crates/simchain-common/src/raw_spam_keys.rs` | New deterministic identity derivation and local P2WPKH signer. |
| `crates/simchain-common/src/lib.rs` | Export the new identity/role/signing API. |
| `crates/spammer/src/raw_transaction_spammer.rs` | Replace private derivation/signing code with common helper; add cooperative pause checks and reconciliation methods. |
| `crates/spammer/src/control.rs` | New lease/status HTTP server and state machine. |
| `crates/spammer/src/runner.rs` | Start control server, integrate pause points, and reconcile both engines before resume. |
| `crates/spammer/src/config.rs` | Parse/validate internal control listen setting if configurable. |
| `crates/reorg/src/spammer_control.rs` | New control client, lease guard, heartbeat, and typed errors. |
| `crates/reorg/src/config.rs` | Parse node2/node3 signer names, control URL/timeout, and coordination mode. |
| `crates/reorg/src/double_spend.rs` | Resolve prevout scripts, classify raw signers, build/sign raw conflicts, source counts, and selected-input tracking. |
| `crates/reorg/src/reorg.rs` | Acquire/release lease around the full rewrite; refresh late-spender exclusions before mining. |
| `crates/reorg/src/runner.rs` | Construct/reuse control client and define once/auto failure behavior. |
| `docker-compose.yml` | Pass wallet names/control settings; expose control port only on the internal network; add reorg-to-spammer dependency only where appropriate. |
| `.env.full.example` | Document coordination settings and raw-engine behavior. |
| `docs/REORGS.md` | Replace the raw-engine limitation with supported semantics, coordination logs, and failure behavior. |
| `docs/SETTINGS.md` | Document settings/defaults and the explicit offline override. |
| `docs/NICE-TO-HAVE.md` | If deferred, add a short entry linking to this plan. If implemented, do not leave it listed as outstanding. |
| `Cargo.toml` files / `Cargo.lock` | Add synchronous HTTP/serde dependencies locally and commit the lockfile update. |

Do not add `[workspace.dependencies]`; repository guidance intentionally forbids it.

## 13. Control-plane dependency choice

Prefer a small synchronous HTTP server/client. One possible dependency set is:

- spammer: `tiny_http` plus `serde` derive,
- reorg: `minreq` plus `serde` derive.

The implementer must verify maintained versions and lock them per crate. A small
hand-written HTTP parser is not recommended; protocol edge cases are not worth saving
one dependency. Do not introduce Tokio/axum unless another planned feature will reuse an
async control plane.

## 14. Failure behavior

Pin these behaviors so failures do not silently degrade the simulation:

| Failure | Required behavior |
| --- | --- |
| Control service unavailable before invalidation (`required`) | Once mode returns an error; auto mode skips this scheduled reorg. Chain remains untouched. |
| Spammer reports wallet-name/engine mismatch | Abort before invalidation and print both expected/actual values. |
| Pause timeout | Abort before invalidation. Do not assume pause. |
| Lease heartbeat fails before invalidation | Abort. |
| Lease heartbeat fails after invalidation | Warn loudly, continue renewing/reacquiring while prioritizing already-built conflicts; final status must report coordination loss. TTL must be long enough that a transient client failure does not immediately resume spam. |
| Raw candidate cannot be signed | Skip candidate before selected count is finalized, or walk later eligible roots until the requested count is reached. |
| Raw replacement fails to mine | Existing post-reorg verification reports `NOT dropped`; do not claim success. |
| Some selected conflicts exceed replacement capacity | Prioritize conflicts, warn with exact count/weight left, and report those originals as not dropped. |
| Reorg process crashes | Lease expires and spammer reconciles before resuming. |
| Reconciliation RPC fails | Keep spammer paused/retrying rather than resume with known-stale branch state. |
| `REORG_SPAMMER_COORDINATION=off` | Emit a prominent warning before invalidation. |

## 15. Logging and observability

Add these operator-visible events:

```text
Raw-spam coordination: pause requested (request_id=..., timeout=180s)
Raw-spam coordination: paused at height 250 (wallets=node2,node3)
Double-spend eligibility: wallet=0, raw-branch=14, raw-floor=66, unsupported=120
Double-spend mode: selected 20 of 80 eligible roots (REORG_DOUBLE_SPEND_PCT=25)
  old -> new (source=raw-floor/node3, descendants pruned=0)
...
Raw-spam coordination: winning chain adopted; requesting reconciliation
Raw-spam coordination: reconciliation complete; spam resumed
```

Never log private keys, WIFs, signatures as secrets, or full raw replacement hex. Public
addresses, scripts, txids, wallet-name labels, counts, and weights are safe for this
regtest audit log.

The current configuration-mismatch warning should change after this feature ships:

- raw mode plus a healthy control handshake is supported and must not warn,
- warn only when coordination is unavailable/disabled or the configured signer set does
  not match the live spammer,
- zero eligible txs can still be a normal information message when no raw root happened
  to land in the orphaned window.

## 16. Tests

### Unit tests — `simchain-common`

- Exact node2/node3 branch/floor address compatibility vectors.
- Same fixed input/output produces byte-identical signatures before/after refactor.
- Branch and floor roles derive different scripts.
- Different wallet names derive different scripts.
- Signer rejects wrong prevout count.
- Signed transaction txid/weight are stable.

### Unit tests — reorg planner

- Classify all-input node2 branch transaction.
- Classify all-input node3 floor transaction.
- Reject mixed raw identities.
- Reject missing prevout/descendant.
- Skip dust after the one-satoshi output reduction.
- Raw replacement has identical inputs, a different output, original fee plus one sat,
  and a valid P2WPKH witness.
- Wallet and raw candidates share one deterministic selection order/percentage.
- Source counts and exclusion sets include raw originals/descendants.
- Late mempool spender and descendants are excluded.

### Unit tests — control state machine

- Pause before a cycle and during a cycle.
- Idempotent duplicate request ID.
- Concurrent different lease receives conflict.
- Heartbeat renews deadline.
- TTL expiry requests reconciliation and resumes.
- Explicit release with and without `chain_changed`.
- Reconciliation failure keeps spam paused.

### Docker integration tests

Use a fresh chain and wait until at least height 212 so the raw engine has completed its
fan-outs and a real spam cycle.

1. **Raw branch permanent drop**
   - `USE_RAW_TX_SPAM=true`, floor pool disabled or identify a branch root,
   - `REORG_DOUBLE_SPEND_PCT=100`, depth 2+
   - assert at least one `raw-branch` mapping,
   - replacement confirmed, original absent from active chain/mempool.
2. **Raw floor permanent drop**
   - floor pool enabled,
   - assert at least one `raw-floor` mapping and recovered replacement output.
3. **Mixed sources**
   - include wallet injection/history and raw spam,
   - verify percentage applies to the ordered union.
4. **Live-spammer race**
   - leave spammer running,
   - verify pause acknowledgement precedes invalidation,
   - no raw sends occur while leased,
   - spam resumes and completes a later cycle.
5. **Control unavailable**
   - stop spammer or block the endpoint,
   - required mode aborts before the target block is invalidated.
6. **Crash recovery**
   - kill reorg container while leased,
   - TTL expires, reconciliation runs, spam resumes.
7. **Auto mode**
   - run multiple reorgs; every lease is released and spam continues between them.
8. **Regressions**
   - pct 0 never contacts control service,
   - wallet engine behaves as today,
   - `empty` ignores double-spend mode,
   - large mempool blocks stay below the weight budget and remain non-empty when enough
     transactions exist.

Record old/new block hashes, old/new txids, source labels, block weights, and post-resume
spammer logs as test artifacts.

## 17. Acceptance criteria

The enhancement is complete only when all are true:

- `REORG_DOUBLE_SPEND_PCT=100` with the default raw engine selects at least one raw root
  in a qualifying orphan window.
- Every logged successful raw mapping has a confirmed replacement on the winning chain.
- Every corresponding original and descendant is absent from the active chain and
  mempool.
- Raw replacement outputs remain controlled/recoverable by the matching spammer
  identity.
- No spam transaction is submitted between pause acknowledgement and lease release.
- The spammer reconciles and successfully completes a later spam cycle without restart.
- Once mode aborts before invalidation when required coordination cannot be acquired.
- Auto mode survives repeated pause/reorg/resume cycles.
- Wallet-engine, pct-zero, and empty-mode regressions pass.
- `cargo ba && cargo ca && cargo fac && cargo tt` passes and `Cargo.lock` is current.
- Documentation no longer says raw spam is categorically ineligible.

## 18. Suggested PR sequence

### PR 1 — behavior-neutral shared signer refactor

- Add common raw identity/signer module.
- Pin address/signature compatibility vectors.
- Refactor spammer to use it.
- No reorg behavior change.

### PR 2 — control lease and reconciliation

- Add spammer control server/state machine.
- Add cooperative pause points.
- Add reconciliation.
- Add reorg control client and required/off behavior.
- Test pause, TTL, and resume independently of double-spend selection.

### PR 3 — raw eligibility and end-to-end reorg support

- Extend planner/classification/signing.
- Add late-spender exclusion.
- Add source logs and update mismatch warning.
- Complete Docker integration tests and documentation.

Keeping the refactor separate makes key-compatibility review possible before concurrency
and consensus-sensitive behavior are mixed into the same diff.

## 19. Reasons to defer

It is reasonable to leave this as a nice-to-have when:

- downstream tests can explicitly switch to `USE_RAW_TX_SPAM=false`,
- only wallet-owned deposits need permanent-drop coverage,
- auto-reorg plus live raw spam is not a target scenario,
- the team does not want to maintain an inter-process lease protocol yet.

The current warning makes the limitation visible, so deferral does not create silent
false confidence.

## 20. Reasons to implement

Implement when:

- the default raw-engine environment must exercise permanent drops without retuning,
- floor-fill or DATA-chain transactions are themselves the target of reorg tests,
- reorgs run automatically while spam remains live,
- reproducible all-default scenarios are more valuable than keeping the processes
  independent.

The key-sharing portion is straightforward because the keys are already deterministic.
The implementation decision should be based primarily on whether live coordination and
post-reorg state recovery justify their maintenance cost.
