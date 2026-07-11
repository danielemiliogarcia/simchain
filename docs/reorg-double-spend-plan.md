# Implementation plan: reorgs that drop transactions permanently (double-spend)

## Status: READY TO IMPLEMENT (written 2026-07-10)

Implements nice-to-have **"3. Reorgs that drop transactions permanently
(double-spend)"** from [NICE-TO-HAVE.md](NICE-TO-HAVE.md). That entry explains the
user-facing rationale; this document is the engineering hand-off: exact scope,
semantics, file-level changes, and the verification plan.

## 1. Goal and non-goals

**Goal:** extend the reorg simulator so a normal, non-`empty` reorg can permanently
drop a configurable fraction of eligible orphaned transactions by mining a conflicting
transaction with the **same inputs** and a **different output** into the replacement
chain.

New setting:

```dotenv
REORG_DOUBLE_SPEND_PCT=0
```

Usage:

```bash
REORG_DOUBLE_SPEND_PCT=50 ./scripts/simulate-reorg.sh 3
REORG_MODE=auto REORG_DOUBLE_SPEND_PCT=25 docker compose --profile reorg up btc-simnet-reorg
```

Semantics:

- `0` keeps today's behavior exactly.
- `1..100` selects that percentage of **eligible** orphaned transactions and replaces
  them permanently on the winning chain.
- the tool logs every original txid and its conflicting replacement txid.

**Non-goals:**

- No automation for **user-owned external-key transactions**. The reorg node does not
  hold the user's keys, so it cannot produce a same-input conflict for them. Users who
  want to test that case still need to broadcast the conflict themselves after an
  `empty` reorg.
- No support in v1 for the spammer's **raw engine** (`USE_RAW_TX_SPAM=true`). Those txs
  are signed by deterministic keys held by the spammer process, not by the reorg node's
  wallet, so the current feature scope stays wallet-only.
- No mempool-broadcast replacement path. The conflicting txs are mined directly into the
  replacement blocks as raw hex; they do not need to satisfy mempool RBF policy.
- No attempt to preserve the exact original output graph. The replacement transaction is
  intentionally a same-input, different-output spend, so descendants of the original
  transaction are supposed to disappear with it.

## 2. Current state (what the plan builds on)

- **Normal reorgs re-mine the returned mempool.** `crates/reorg/src/reorg.rs` invalidates
  the branch, lets orphaned txs return to the mempool, then re-reads the mempool live
  for each replacement block and mines evenly-sized slices with `generateblock`.
- **`empty` mode already models the temporary-drop case.** `./scripts/simulate-reorg.sh
  <depth> empty` mines empty replacement blocks and leaves orphaned txs unconfirmed.
  That is the confirmed -> 0-conf -> maybe later re-confirmed scenario, not the
  permanent-drop scenario.
- **The reorg tool already has wallet access on the reorg node.**
  `crates/reorg/src/wallet.rs` resolves `REORG_WALLET_NAME` (falling back to the first
  loaded wallet) and uses it to inject `REORG_ADDS_NEW_TXS` transactions.
- **The reorg tool can already mine exact block contents.**
  `crates/reorg/src/chain.rs::mine_exact` calls `generateblock`, which accepts an
  ordered list of txids. Bitcoin Core also allows raw transaction hex in that list, so
  the reorg tool can mine a non-mempool conflicting tx directly.
- **The default spam mode is the raw engine, not the wallet engine.**
  `docker-compose.yml` defaults `USE_RAW_TX_SPAM=true`, so with stock settings there may
  be **zero eligible transactions** to double-spend. This is not a bug; it is a scope
  consequence that must be documented clearly.
- **The current crate layout is small and clean.**
  `crates/reorg/src` only has `chain.rs`, `config.rs`, `reorg.rs`, `runner.rs`,
  `wallet.rs`. The new feature should be split into a dedicated module instead of
  bloating `reorg.rs`.

## 3. Pin the exact semantics before coding

The nice-to-have description is directionally right, but the implementation needs the
rules below pinned down explicitly.

### What counts as an eligible transaction

An orphaned tx is eligible for permanent replacement only if **all** of the following are
true after invalidation:

- the reorg wallet recognizes it as its own transaction,
- the wallet can sign a same-input raw replacement for it,
- every input spends a UTXO that exists on the rolled-back chain (`gettxout(...,
  include_mempool=false)` succeeds),
- it is therefore a **root** returned transaction, not a descendant of another returned
  tx.

This root-only rule matters. If a tx spends the output of another orphaned tx, the
correct permanent-drop behavior is to double-spend the ancestor and let the descendant
die with it, not to attempt to rebuild the descendant independently.

### What the percentage applies to

`REORG_DOUBLE_SPEND_PCT` applies to the count of **eligible root wallet txs**, not to all
orphaned txs.

If `eligible > 0` and `pct > 0`, select:

```text
max(1, floor(eligible * pct / 100))
```

This avoids the surprising "25% of 3 selected 0 txs" no-op while keeping the feature
deterministic.

### Selection order

Selection should be deterministic, not random:

- walk the orphaned blocks oldest-first,
- within a block, keep tx order as mined,
- take the first `selected_count` eligible root txs.

Determinism matters for reproducible tests and for manual debugging against the log.

### `empty` mode interaction

`REORG_DOUBLE_SPEND_PCT` is ignored in `empty` mode, with an explicit log line. `empty`
means empty; mixing "mine no txs" and "mine conflicting txs" in one run is the wrong
surface.

## 4. User-visible v1 behavior

With the feature on, a normal reorg should log something like:

```text
Double-spend mode: selected 3 of 5 eligible wallet txs (REORG_DOUBLE_SPEND_PCT=50)
  old_txid_1 -> new_txid_1 (2 descendants pruned)
  old_txid_2 -> new_txid_2 (0 descendants pruned)
  old_txid_3 -> new_txid_3 (1 descendant pruned)
```

After the reorg:

- the original selected txids are not in the active chain,
- they are not left sitting in the mempool,
- their descendants are also gone if they depended on the replaced outputs,
- the logged conflicting txids are mined into the replacement branch instead.

If there are no eligible txs, the reorg still succeeds normally and logs why:

- wrong spam engine (`USE_RAW_TX_SPAM=true`),
- wrong reorg node / wallet,
- no wallet txs happened to be in the orphaned window,
- or all wallet txs in the window were descendants rather than independently
  replaceable roots.

## 5. Change 1: add config surface and env plumbing

### `crates/reorg/src/config.rs`

Add a new field:

```rust
pub double_spend_pct: u8,
```

Parse:

- env key: `REORG_DOUBLE_SPEND_PCT`
- default: `0`
- allowed range: `0..=100`

Parsing belongs locally in the reorg crate; it is not shared across tools.

### `docker-compose.yml`

Pass the setting into `btc-simnet-reorg`:

```yaml
- REORG_DOUBLE_SPEND_PCT=${REORG_DOUBLE_SPEND_PCT:-0}
```

### Docs / examples

- add the variable to `.env.full.example`
- add it to `docs/SETTINGS.md`
- mention it in `docs/REORGS.md`

`scripts/simulate-reorg.sh` needs no change: env vars already flow through the wrapper.

## 6. Change 2: add a dedicated `double_spend.rs` module

Create:

```text
crates/reorg/src/double_spend.rs
```

and register it from `crates/reorg/src/main.rs`.

This module should own:

- eligibility detection,
- conflicting raw-tx construction,
- descendant exclusion planning,
- log formatting for the replacement summary.

Suggested types:

```rust
pub struct DoubleSpendPlan {
    pub replacements: Vec<ReplacementTx>,
    pub excluded_mempool_txids: HashSet<Txid>,
}

pub struct ReplacementTx {
    pub original_txid: Txid,
    pub replacement_txid: Txid,
    pub raw_hex: String,
    pub pruned_descendants: Vec<Txid>,
}
```

Keeping this in its own module matters because the logic is conceptually separate from
"how to invalidate and win the chain race."

## 7. Change 3: refactor wallet resolution into a reusable helper

Today `wallet.rs` resolves the wallet ad hoc inside `inject_transactions`. That should be
factored into a shared helper because the double-spend planner needs the exact same
resolution semantics.

`crates/reorg/src/wallet.rs` should expose something like:

```rust
pub fn resolve_wallet(node: &Client) -> Option<(String, Client)>
```

Behavior:

- prefer `REORG_WALLET_NAME`,
- fall back to the first loaded wallet exactly as today,
- log once if the preferred wallet is not loaded,
- return `None` if no wallet is loaded.

`inject_transactions` should reuse it. The new double-spend planner should also reuse
it, so the feature never disagrees with the existing "fresh tx injection" wallet choice.

## 8. Change 4: build the replacement plan from the orphaned branch

This is the core of the feature.

### Capture the orphaned tx universe

Before invalidation, the reorg tool already captures a human summary of the last
`depth + 2` blocks through `last_blocks()`. Extend the data capture so the reorg code
also has the full txid list for the last `depth` blocks being orphaned.

Add a helper in `chain.rs`, for example:

```rust
pub struct BranchBlock {
    pub height: u64,
    pub hash: BlockHash,
    pub txids: Vec<Txid>,
}
```

and a function that returns the exact branch slice to be replaced.

### Build the plan after invalidation

The double-spend plan must be built **after** invalidation, because eligibility depends on
the rolled-back chain state:

- root inputs become visible again as on-chain UTXOs,
- descendants stay non-root,
- orphaned txs are back in the mempool, so descendant closure can be queried.

Planner algorithm:

1. Resolve the reorg wallet client.
2. Flatten the orphaned branch txids oldest-first, excluding coinbases.
3. For each txid:
   - skip it if the wallet does not recognize it (`gettransaction` on the wallet-scoped
     client fails),
   - fetch the verbose raw transaction from the node,
   - require every input's prevout to exist in the rolled-back chain UTXO set via
     `gettxout(prev_txid, vout, false)`,
   - compute the input total from those prevouts,
   - compute the original absolute fee as `input_total - sum(outputs)`,
   - get a fresh wallet address,
   - build a raw tx with the **same inputs** and a **single output** paying
     `input_total - original_fee` to that fresh address,
   - sign it with `signrawtransactionwithwallet`,
   - if signing is incomplete or the output would be dust/non-positive, skip it.
4. From the eligible list, select the first `selected_count`.
5. For each selected original txid, query its mempool descendant closure and add those
   txids to an exclusion set.
6. The final exclusion set is:
   - every selected original txid,
   - every mempool descendant of those originals.

That exclusion set prevents the replacement chain from accidentally mining descendants of
the txs it is intentionally killing.

## 9. Why the replacement txs should be mined as raw hex, not broadcast

Do **not** try to `sendrawtransaction` the conflicts into the mempool first.

Reason:

- the original orphaned tx may not signal RBF,
- mempool replacement policy is narrower than "valid in a block",
- the goal here is a replacement **chain**, not a replacement **mempool entry**.

Instead, pass the conflicting raw transactions directly to `generateblock` as raw hex.
Bitcoin Core accepts a mixed ordered list of mempool txids and raw transaction hex in
that RPC.

This keeps the feature aligned with the real use case: a miner can mine a conflicting
transaction in the winning chain whether or not your node would have accepted it into its
mempool as an opt-in RBF replacement.

## 10. Change 5: extend block assembly to handle mixed tx sources

`crates/reorg/src/chain.rs::mine_exact` currently accepts only `&[Txid]`. That is no
longer enough.

Introduce a mixed item type, for example:

```rust
pub enum BlockTx {
    Mempool(Txid),
    RawHex(String),
}
```

Then:

- update `mine_exact` to serialize either kind into the `generateblock` argument list,
- keep the existing stale-mempool retry logic for `Mempool(Txid)` items,
- retain `RawHex` items across the retry (they are not supposed to be filtered by live
  mempool presence),
- if a retry still fails, degrade to a block containing just the raw replacements before
  giving up entirely.

Add a filtered mempool helper too:

```rust
pub fn live_mempool_topo_filtered(node: &Client, excluded: &HashSet<Txid>) -> Result<Vec<Txid>, _>
```

This should behave like today's `live_mempool_topo`, but remove:

- selected originals,
- descendants of selected originals.

## 11. Change 6: integrate the planner into the main reorg loop

`crates/reorg/src/reorg.rs` should change in this order:

1. capture the orphaned branch metadata before invalidation,
2. invalidate the target block as today,
3. log the returned mempool count as today,
4. if `empty_mode`, skip all double-spend logic,
5. otherwise, if `double_spend_pct > 0`, build the `DoubleSpendPlan`,
6. inject `REORG_ADDS_NEW_TXS` as today,
7. mine the replacement blocks from:
   - a queue of raw conflicting txs,
   - plus the live filtered mempool.

### Spreading the replacements across blocks

Do not dump every conflicting tx into the first replacement block. Spread them the same
way the current reorg code spreads live mempool txs:

- `double_spends_left.div_ceil(blocks_left)` raw conflicts for this block,
- `filtered_mempool_left.div_ceil(blocks_left)` txids for this block.

That keeps the replacement branch shape closer to the current "mine the live mempool
evenly" behavior and avoids a lopsided first block.

## 12. What needs no behavior changes

- **`empty` reorgs:** unchanged, except for the explicit "double-spend ignored in empty
  mode" log line.
- **Witness-based network adoption:** unchanged. Once the conflicting replacement block
  is connected, the original txs are no longer mempool-valid on the reorg node, so the
  extra race-winning blocks mined later do not resurrect them.
- **Mining controller integration:** unchanged. The controller only sees the new chain;
  it does not need to know why certain txids vanished.
- **`REORG_ADDS_NEW_TXS`:** unchanged in the same run. Those txs are injected **after**
  invalidation and are mined into the new branch; they are not candidates to be
  permanently dropped in that same reorg.

## 13. Logging and observability

Add one dedicated log section after the block-replacement summary:

```text
--- Permanently dropped transactions ---
old_txid -> new_txid (descendants pruned: N)
...
```

Also log the top-level summary early:

- configured pct,
- eligible count,
- selected count,
- if selected count is zero, why.

This log output is the primary operator-facing proof that the feature did what it was
supposed to do, and it is the anchor for manual verification and future tests.

## 14. Documentation updates (same PR)

- **`docs/REORGS.md`**
  - explain the new permanent-drop mode,
  - note that `empty` already covers temporary drop,
  - document that wallet-engine spam is the easiest way to exercise this feature.
- **`docs/SETTINGS.md`**
  - add `REORG_DOUBLE_SPEND_PCT`,
  - explicitly state that it applies only to eligible orphaned wallet txs on the reorg
    node.
- **`.env.full.example`**
  - add `REORG_DOUBLE_SPEND_PCT=0`,
  - add one warning comment that with default `USE_RAW_TX_SPAM=true` there may be no
    eligible spam txs to conflict.
- **`docs/NICE-TO-HAVE.md`**
  - remove item #3 once shipped and renumber.

`README.md` does not need more than a short pointer to `REORGS.md` if desired; the
feature belongs primarily in the reorg docs.

## 15. Verification plan

### Targeted unit tests

Add small unit tests around the pure helpers in `double_spend.rs` / `chain.rs`:

1. percentage selection:
   - `pct=0` selects 0,
   - `pct>0` with `eligible>0` selects at least 1,
   - `pct=100` selects all.
2. deterministic ordering:
   - candidate order follows orphaned block order, not txid sort.
3. exclusion set logic:
   - selected original + descendants are both excluded from mempool mining.

Do **not** attempt a fake-RPC unit test for signing or generateblock behavior; those are
better verified end-to-end on the running simnet.

### Manual, in order

1. Baseline compatibility:
   - leave `REORG_DOUBLE_SPEND_PCT=0`,
   - run `./scripts/simulate-reorg.sh 3`,
   - confirm behavior matches today exactly: no double-spend log section, returned txs
     can be re-mined normally.

2. Empty mode ignore:
   - set `REORG_DOUBLE_SPEND_PCT=100`,
   - run `./scripts/simulate-reorg.sh 3 empty`,
   - confirm the tool logs that double-spend mode is ignored and orphaned txs remain in
     the mempool as the existing chaos reorg does.

3. Wallet-engine setup:
   - start the simnet with `USE_RAW_TX_SPAM=false`,
   - let the spammer and miners run long enough to put wallet-created txs into recent
     blocks on the reorg node,
   - optionally set a fixed interval long enough that blocks are easy to inspect.

4. 100% permanent drop:
   - set `REORG_DOUBLE_SPEND_PCT=100`,
   - run `./scripts/simulate-reorg.sh 3`,
   - record the logged `old -> new` txid pairs,
   - confirm the new txids appear in the replacement blocks,
   - confirm the old txids are neither in the active chain nor left in the mempool.

5. Descendant pruning:
   - create a scenario where at least one eligible wallet tx has descendants in the
     orphaned window,
   - run the reorg with `REORG_DOUBLE_SPEND_PCT=100`,
   - confirm those descendants are also absent from the replacement chain and mempool.

6. Partial percentage:
   - set `REORG_DOUBLE_SPEND_PCT=50`,
   - run the same style reorg,
   - confirm some eligible txs are replaced and the rest are re-mined normally.

7. Wrong-engine / zero-eligible path:
   - switch back to default `USE_RAW_TX_SPAM=true`,
   - run with `REORG_DOUBLE_SPEND_PCT=100`,
   - confirm the reorg still succeeds and logs "0 eligible wallet txs" rather than
     silently appearing broken.

8. Auto mode:
   - run `REORG_MODE=auto` with a non-zero `REORG_DOUBLE_SPEND_PCT`,
   - confirm multiple reorg cycles keep working and the log remains readable.

## 16. Risks and edge cases

- **Default raw spam means zero candidates is common.** This is the biggest operator
  surprise if it is not documented aggressively.
- **Descendants must be excluded, not just originals.** Mining a descendant of a
  deliberately replaced tx would make the block invalid. This is the most important
  correctness condition in the implementation.
- **Some wallet txs will still be ineligible.** If a wallet tx is itself a descendant of
  another orphaned tx, or if rebuilding it would leave only dust, it must be skipped.
- **The scope is signability-based, not provenance-perfect.** In practice v1 will
  permanent-drop any eligible orphaned tx the reorg wallet can sign, not only txs that
  were "spam" in some semantic sense. That is acceptable and simpler.
- **Mixed raw-hex + txid block assembly complicates retry logic.** The current
  `mine_exact` retry path assumes every selected item must already be in the mempool; the
  new path must keep raw replacements intact while filtering only stale txids.

## 17. Effort and change list

Medium. No new crate, no new image, no change to the mining or spammer binaries.

| File | Change |
| --- | --- |
| `docker-compose.yml` | Pass `REORG_DOUBLE_SPEND_PCT` into `btc-simnet-reorg` |
| `crates/reorg/src/main.rs` | Register `double_spend` module |
| `crates/reorg/src/config.rs` | Parse and store `REORG_DOUBLE_SPEND_PCT` |
| `crates/reorg/src/wallet.rs` | Extract reusable wallet-resolution helper |
| `crates/reorg/src/chain.rs` | Add orphan-branch metadata helper, filtered mempool helper, mixed raw/txid mining |
| `crates/reorg/src/double_spend.rs` | New planner for eligibility, raw replacement construction, descendant exclusion, and logs |
| `crates/reorg/src/reorg.rs` | Orchestrate plan creation and mixed-content replacement mining |
| `.env.full.example` | Add `REORG_DOUBLE_SPEND_PCT` with warning comment |
| `docs/SETTINGS.md` | Document the new setting and its wallet-only scope |
| `docs/REORGS.md` | Document permanent-drop mode and the raw-engine caveat |
| `docs/NICE-TO-HAVE.md` | Remove item #3 once implemented |

## 18. Recommended implementation order

1. Add the config/env/documentation surface.
2. Extract wallet resolution into a shared helper.
3. Add `double_spend.rs` with pure selection helpers and unit tests.
4. Extend `chain.rs` to support mixed raw-hex / txid block mining.
5. Wire the planner into `reorg.rs`.
6. Run the manual verification sequence with wallet-engine spam.

That order keeps the hard parts isolated: eligibility planning first, mixed block
assembly second, orchestration last.
