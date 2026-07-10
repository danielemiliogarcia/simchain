# Implementation plan: fee-market simulation in the spammer (opt-in)

## Status: PARKED (2026-07-10) — do not implement until a concrete need appears

Decision after design review: the cost/benefit does not justify building this now.

- **Cost is bigger than it looks.** The mode touches every fee path of a tuned,
  working engine (`shape_fee`, `per_tx_required`, floor fills, fan-out, funding
  pulls). The floor-pool / fill-ratio equilibrium took real tuning to get airtight,
  and market mode interacts with all of it. It also introduces new long-run behaviors
  to babysit: low-fee branches perma-stall at the 25-tx ancestor limit, their capital
  stays locked in stuck chains, and the mempool-deficit measurement then counts dead
  weight — over long runs the effective market drifts toward the top of the ladder
  unless re-tuned. Every future spammer change would have to reason about two fee
  modes.
- **Benefit is narrow.** It only pays off if the project under test does fee
  estimation (`estimatesmartfee`) or fee-bumping logic. Indexers, confirmation
  trackers and reorg handling are already covered by flat floor + full blocks +
  standing backlog. Partial substitutes exist: `ENABLE_SPAM_REPLACES` (RBF traffic),
  `FALLBACK_FEE` (price level), and the two existing fee buckets (floor + bulk
  premium). The CPFP constraint (§1) forces the per-branch design, so the spread is
  ~45 discrete levels anyway, not a continuum.

**Revisit when** a downstream project concretely needs fee-estimation or fee-bumping
tests. The expensive thinking is done and recorded below: the CPFP-rescue analysis
(§1), the per-branch ladder design, and the funding-pull deadlock fix (§3.3).

Implements nice-to-have feature 2, **as a selectable mode**: the current flat-floor
behavior stays the default and remains byte-for-byte unchanged; a new market mode
spreads spam fee rates across a configurable range so `estimatesmartfee`, the mempool
explorer's fee histogram, and RBF/fee-bumping logic in projects under test become
meaningful.

This plan supersedes the implementation sketch in `nice-to-have.md` item 2: that sketch
(pass `fee_rate` to `send_to_address`/`send_many`) predates the raw engine, which is now
the default and computes fees itself (`fee_from_vsize` in
`spammer/src/raw_transaction_spammer.rs`). Size/output-count variance from the sketch
already exists (`SPAM_TX_DATA_MIN/MAX_BYTES` log-uniform draw, `SPAM_SENDMANY_OUTPUTS`),
so the only genuinely new work is fee-rate variance.

## 1. The one hard design problem: unconfirmed chains defeat naive per-tx fees

Raw-engine spam txs chain off their own unconfirmed change (each branch in `utxos` is
the tip of an in-mempool chain, see `send_shape`). Block assembly scores by **ancestor
feerate**: if fee rates were sampled per tx, a 50 sat/vB tx landing on a branch whose
tip is a 1 sat/vB tx forms a CPFP package and drags the low-fee ancestors into the
block. Low-fee txs would NOT genuinely wait — the whole point of the feature would be
silently defeated, while the histogram looked fine.

**Solution: per-branch fee rates.** Each branch is assigned a fixed fee rate, so every
chain is fee-homogeneous — no CPFP rescue, no ancestor-score penalty. Consequences,
all desirable and mainnet-like:

- Low-fee branches accumulate unconfirmed chains until the 25-ancestor limit and stall
  (sends fail with `too-long-mempool-chain`; the engine already tolerates per-branch
  send failures and round-robins on). A standing low-fee backlog builds in the mempool.
- High-fee branches confirm every block and keep spamming, so blocks stay full and are
  dominated by the top of the fee ladder.
- The fee histogram shows a persistent spread; confirmation time genuinely correlates
  with fee rate, which is what feeds `estimatesmartfee`.

The rate for branch index `i` is drawn **log-uniformly** in
`[SPAM_FEE_MIN, SPAM_FEE_MAX]` with the same deterministic multiplicative-hash trick
already used by `draw_data_size` (no `rand` dependency — keep the crate RNG-free):

```rust
fn branch_rate(&self, idx: usize) -> f64 {
    match self.fee_mode {
        FeeMode::Flat => self.fee_rate_sat_vb, // exactly today's behavior
        FeeMode::Market => {
            let h = (idx as u32).wrapping_mul(2_654_435_761);
            let frac = h as f64 / u32::MAX as f64;
            self.fee_min * (self.fee_max / self.fee_min).powf(frac)
        }
    }
}
```

Log-uniform puts most branches at low rates and a few at high rates (mainnet-shaped).
Hash-of-index (rather than `i/(n-1)`) keeps each branch's rate stable when the pool
shrinks (`utxos.remove(idx)` shifts later indices; approximate stability is fine — a
reshuffled branch just changes fee band, its existing chain is already priced).
The set of rates is deterministic across restarts, which suits the resync/recovery
story.

`draw_data_size` (tx size spread) is orthogonal and unchanged: in market mode blocks
contain big-cheap, big-expensive, small-cheap and small-expensive txs.

## 2. Settings

| Setting | Default | Meaning |
|---|---|---|
| `SPAM_FEE_MODE` | `flat` | `flat`: everything anchors on `FALLBACK_FEE` exactly as today. `market`: per-branch fee ladder. |
| `SPAM_FEE_MIN` | `1` | Market mode: bottom of the ladder, sat/vB. Floor fills also pay this. Must be ≥ the relay floor (1 sat/vB at the default `MIN_RELAY_TX_FEE`). |
| `SPAM_FEE_MAX` | `50` | Market mode: top of the ladder, sat/vB. |

Parsing/validation in `spammer/src/main.rs`, same style as the existing settings:

- `SPAM_FEE_MODE` accepts `flat` / `market`, anything else panics with a clear message.
- In market mode: panic if `SPAM_FEE_MIN <= 0`, `SPAM_FEE_MAX < SPAM_FEE_MIN`; warn
  (println) if `SPAM_FEE_MIN` is below the relay floor implied by
  `MIN_RELAY_TX_FEE` (the spammer doesn't read that var today — just warn on
  `SPAM_FEE_MIN < 1.0` against the compose default).
- In market mode, warn if `FALLBACK_FEE`-in-sat/vB lies outside `[MIN, MAX]` — not an
  error (FALLBACK_FEE still drives node wallets), just surprising.
- In flat mode, warn once if `SPAM_FEE_MIN`/`SPAM_FEE_MAX` are set (they are ignored).

`SPAM_FEE_MODE=market` is meaningful for **both raw-engine modes** (DATA/HYBRID and
OUTPUT), since both use the branch pool. The **node-wallet engine**
(`USE_RAW_TX_SPAM=false`) does not support it: print a warning and run flat. Extending
the wallet engine would require `settxfee` round-trips per rate and reintroduce
wallet-lock serialization — out of scope, note it in SETTINGS.md.

No node policy flags change (project rule: no relay/mempool/capacity flag drift —
this feature is purely spammer behavior).

## 3. Code changes, file by file

### 3.1 `spammer/src/raw_transaction_spammer.rs`

1. Add `enum FeeMode { Flat, Market }` and fields `fee_mode: FeeMode`,
   `fee_min: f64`, `fee_max: f64` to `RawSpammer`; extend `RawSpammer::new` to take
   them (both call sites are in `main.rs`).
2. Add `branch_rate(idx)` as in §1.
3. Thread the rate through fee computation. Today's chain is
   `send_shape → shape_fee → {fee_from_vsize | bulk_fee_from_vsize}`. Change
   `shape_fee(&self, shape, rate_sat_vb: f64)`:
   - Flat mode: callers pass `self.fee_rate_sat_vb`; keep the DATA-mode
     `BULK_FEE_PREMIUM_SAT_VB` exactly as today.
   - Market mode: callers pass `branch_rate(idx)`; **no premium** (the ladder replaces
     it — the premium existed only to order bulk above the flat floor).
   In `send_shape`, pick the branch first (`next_branch`), then compute the fee with
   that branch's rate. Note the ordering problem this creates with `per_tx_required`
   (next point).
4. `per_tx_required` (branch affordability, refill sizing) must be conservative:
   in market mode compute it with `fee_max`. Slightly overfunds low-fee branches;
   harmless. `usable_branches`/`next_branch` keep using this single conservative
   `required` value — do NOT make affordability rate-dependent per branch, it isn't
   worth the complexity.
5. Floor pool: `fill_fee()` uses `fee_min` in market mode (flat: unchanged
   `fee_rate_sat_vb`). The pool keeps its job — sealing residual packing gaps at the
   bottom of the market. Everything else in `floor_round`/`ensure_pool_funds` is
   untouched.
6. Fan-out / refill safety: `consolidation_fee` currently pays
   `fee_rate × FANOUT_FEE_MULTIPLIER`. In market mode it must pay
   `fee_max × FANOUT_FEE_MULTIPLIER` — a refill tx priced mid-ladder would compete
   with the very backlog it replenishes and could stall the engine (the code blocks
   waiting for fan-out confirmation).
7. RBF bumps (`bump_spam_txs`): no change needed — it doubles the recorded fee of the
   specific tx (`SentSpam.fee`), which is already per-tx data. In market mode bumps
   naturally jump histogram buckets, which is realistic RBF traffic.
8. Per-cycle fee summary log (nice-to-have step 3). Cheapest honest version: in
   `hybrid_round`/`output_round`, track min/max rate and vsize-weighted mean of the
   rates actually used this cycle, and print one line, e.g.
   `Node 2 => Fees offered this cycle: 1.2..48.7 sat/vB, weighted mean 9.3 (market mode)`.
   Flat mode keeps today's log lines untouched.

### 3.2 `spammer/src/main.rs`

1. Parse the three new settings with validation as in §2 (near the existing
   `FALLBACK_FEE` block, `main.rs:152`).
2. Pass mode/min/max into both `RawSpammer::new` call sites.
3. Startup banner: extend the existing `Spam engine: ...` println with either
   `fee mode: flat, floor N sat/vB` or `fee mode: market, N..M sat/vB across K branches`.
4. Wallet engine path (`USE_RAW_TX_SPAM=false`): if market mode requested, print
   `WARNING: SPAM_FEE_MODE=market requires the raw engine (USE_RAW_TX_SPAM=true); running flat` and proceed flat.

### 3.3 Funding-pull confirmation risk (must handle)

`ensure_funds`/`ensure_pool_funds` pull from the miner wallet with
`send_to_address(...)` at wallet-default pricing (≈`FALLBACK_FEE`) and then **block
until 1 confirmation**. In market mode with `SPAM_FEE_MAX` well above `FALLBACK_FEE`
and full blocks, that funding tx can be outbid indefinitely → engine deadlock.

Fix: in market mode, price the pull at the top of the ladder. `bitcoincore-rpc`'s
`send_to_address` wrapper has no `fee_rate` arg, so issue the RPC raw at both pull
sites, replacing the wrapper call:

```rust
let txid: bitcoincore_rpc::bitcoin::Txid = self.wallet.call(
    "sendtoaddress",
    &[json!(self.address.to_string()), json!(pull.to_btc()),
      json!(null), json!(null), json!(false), json!(null),
      json!(null), json!(null), json!(false),
      json!(pull_fee_rate_sat_vb)],  // fee_rate, sat/vB (Core 0.21+)
)?;
```

with `pull_fee_rate_sat_vb = fee_max.ceil() + 1.0` in market mode; in flat mode keep
the existing wrapper call untouched (zero behavioral drift).

Why this is sufficient: the pull becomes the highest-feerate standalone tx in the
mempool (~141 vB), and block assembly always takes the top of the mempool by ancestor
feerate, so it is mined in the very next block — deterministic, not probabilistic.
Cost is negligible: 141 vB × (MAX+1) sat/vB ≈ 7,200 sat at the default MAX=50, paid
once per refill (hundreds of blocks apart), against a ~2,550 BTC miner wallet.
An RBF-watchdog alternative (send at default rate, `bumpfee` after N blocks) was
considered and rejected: more code, nondeterministic wait, no additional benefit.

Safety net (both modes, cheap insurance): the two confirmation wait loops after a
pull currently spin forever. Add a loud periodic warning — if the pull is still
unconfirmed after ~20 blocks, print the txid and the configured fee settings every
further 10 blocks so a misconfiguration (e.g. an absurd `SPAM_FEE_MAX` clashing with
`-maxtxfee`) surfaces in the logs instead of presenting as a silent hang. Keep
waiting; do not abort.

### 3.4 `docker-compose.yml`

Add to the `btc-simnet-spammer` environment block (with the same comment style as
neighbors):

```yaml
      # Fee pricing mode. flat (default): everything anchors on FALLBACK_FEE,
      # exactly the historical behavior. market: raw-engine branches get fee
      # rates spread log-uniformly in [SPAM_FEE_MIN, SPAM_FEE_MAX] sat/vB, so
      # fee histograms / estimatesmartfee / RBF become meaningful and low-fee
      # txs genuinely wait when blocks are full.
      - SPAM_FEE_MODE=${SPAM_FEE_MODE:-flat}
      - SPAM_FEE_MIN=${SPAM_FEE_MIN:-1}
      - SPAM_FEE_MAX=${SPAM_FEE_MAX:-50}
```

### 3.5 `.env.example`, `.env.full.example`, `SETTINGS.md`

- `.env.example`: add the three settings commented-out under the spam recipe with a
  one-line pointer ("uncomment for a fee market instead of a flat floor").
- `.env.full.example`: add with full comments, defaults matching compose.
- `SETTINGS.md`:
  - Add the three settings to the Spammer table (line ~178).
  - New subsection after "The fee floor" (line ~280): **"Fee market mode"** — explain
    flat vs market, the per-branch ladder, why chains must be fee-homogeneous (the
    CPFP rescue problem), that floor fills sit at `SPAM_FEE_MIN`, that
    `FALLBACK_FEE` still governs node wallets and the flat mode, interaction with
    `SPAM_FILL_BLOCK_RATIO` (ratio ≥ 1 required for real competition; at ratio < 1
    everything confirms next block regardless of fee), and a note that market mode
    needs the raw engine.
  - Update "The fee market: what spam pays..." section (line ~105) to mention the new
    mode exists and link the new subsection.

### 3.6 `nice-to-have.md`

Per repo convention: **delete** feature 2 entirely (never mark done), renumber
features 3–5 to 2–4, and fix every reference to the count/numbers ("five proposed
features" → "four", the list in the Simulations section, "Pairs well with feature 1"
cross-references). Also update the "Fee-market pressure" bullet under Simulations to
point at the new `SPAM_FEE_MODE` setting instead of describing it as future work.

## 4. Verification

Build note: code changes need an image rebuild **in the same compose project**
(`docker compose build btc-simnet-spammer` from the repo root, then `up -d`); a stale
image silently runs old code. Use `/usr/bin/docker` for unmangled output.

1. **Flat-mode regression (default, no .env changes):** run the stack, confirm the
   startup banner says flat, blocks fill exactly as before, every mempool entry sits
   at the floor (± the bulk premium):
   `docker exec btc-simnet-node1 bitcoin-cli -regtest ... getrawmempool true | jq '[.[] | .fees.base]' `
   — same two buckets as today.
2. **Market mode:** set `SPAM_FEE_MODE=market`, `SPAM_FILL_BLOCK_RATIO=3`. Check:
   - `getrawmempool true | jq '[.[] | (.fees.base / .vsize * 1e8)] | min, max'` spans
     roughly `[SPAM_FEE_MIN, SPAM_FEE_MAX]`.
   - Ancestor homogeneity (the CPFP guard):
     for a sample of entries, `.fees.ancestor / .ancestorsize ≈ .fees.base / .vsize`.
   - Low-fee txs wait: pick a min-rate txid, confirm it stays unconfirmed across
     several blocks while high-rate txs confirm next block.
   - `estimatesmartfee 2` vs `estimatesmartfee 10` after ~30 blocks: the 2-block
     estimate should be meaningfully higher.
   - Mempool explorer (`--profile mempool`): histogram shows a spread, not one bar.
   - Engine health over ≥50 blocks: no deadlock in `ensure_funds`/`ensure_pool_funds`
     (funding pulls confirm), warnings about branch exhaustion are occasional not
     constant, blocks stay full.
3. **Wallet engine guard:** `USE_RAW_TX_SPAM=false` + `SPAM_FEE_MODE=market` → warning
   printed, spam runs flat.
4. **Validation errors:** `SPAM_FEE_MAX < SPAM_FEE_MIN` panics with the message;
   `SPAM_FEE_MODE=bogus` panics.
5. `cargo build` in `spammer/` must exit 0 (check the exit code, not the output).

Do not commit or stage anything — the user manual-tests first and commits himself.

## 5. Explicitly out of scope

- Log-normal or other pluggable distributions (`SPAM_FEE_DIST`): the deterministic
  log-uniform ladder covers the use case; a distribution knob can be added later
  without touching the settings above.
- Fee-market support in the node-wallet engine.
- Time-varying fee levels (rush hours / bursts): belongs to the scenario engine
  (nice-to-have feature 3).
- `SPAM_OUTPUTS_MAX` from the original sketch: size/output variance already exists
  (`SPAM_TX_DATA_MIN/MAX_BYTES`, `SPAM_SENDMANY_OUTPUTS`).
