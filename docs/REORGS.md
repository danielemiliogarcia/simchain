# Simulating Reorgs

The reorg simulator (a Rust container using only bitcoind RPC calls) invalidates the last *N* blocks on a miner node and mines *N+1* replacements, so the new chain is strictly longer and **the whole network reorgs to it**. Transactions from the orphaned blocks fall back to the mempool; each replacement block is filled by re-reading the mempool live and mining a slice of it with `generateblock`, like the winning chain of a real reorg, so reorged blocks are not empty. Reading the mempool fresh for each block means an RBF replacement that evicts an orphaned tx mid-reorg (e.g. with `ENABLE_SPAM_REPLACES=true`) is picked up automatically. On top of the returned txs it seeds `REORG_ADDS_NEW_TXS` fresh wallet transactions into the mempool first, modelling a node that received transactions its peers have not yet seen. It prints each block's hash and tx count before/after plus a replaced-blocks summary.

When fresh transactions are injected, the log includes the number created and one
sample txid. Search that txid in the explorer (or with Bitcoin Core RPC) to verify
that it appears only on the winning branch. The replaced-blocks summary is also an
audit index: Bitcoin Core retains stale blocks in its datadir and they remain
queryable by their old hash (unless the node is pruned or its volumes are deleted).
The explorer's height timeline shows only the active chain, so retain the logged old
hash when you want to inspect a replaced block directly.

## One-Shot Reorg

Pass `empty` to mine **empty** replacement blocks instead (a chaos reorg that leaves the orphaned txs unconfirmed): `./scripts/simulate-reorg.sh 3 empty`. It is a per-run argument, not a setting, so a real reorg and an empty one can be issued against the same running chain.

```bash
./scripts/simulate-reorg.sh 3
# equivalent to:
docker compose run --rm btc-simnet-reorg 3     # depth defaults to REORG_DEPTH (3)
./scripts/simulate-reorg.sh 3 empty            # chaos: mine empty replacement blocks
```

## Permanent Drop (double-spend)

By default a reorg re-mines the orphaned transactions with the **same txids**, so a user's transaction only changes block hash/height, it never loses a confirmation. The `empty` mode above models the *temporary* drop (confirmed → 0-conf, re-confirmable). `REORG_DOUBLE_SPEND_PCT=1..100` models the *permanent* drop: for that percentage of the **eligible orphaned wallet txs on the reorg node**, the tool mines a same-input, different-output conflict into the replacement chain, so the originals become permanently invalid and can never re-confirm. This is the outcome exchanges, custody watchers and payment processors must detect: *"my confirmed deposit is gone forever."*

```bash
REORG_DOUBLE_SPEND_PCT=100 ./scripts/simulate-reorg.sh 3        # drop all eligible
REORG_DOUBLE_SPEND_PCT=50  ./scripts/simulate-reorg.sh 3        # drop half, re-mine the rest
```

It logs the configured percentage, the eligible/selected counts, and every `old_txid -> new_txid` pair (with how many descendants each replacement pruned), so the drop is auditable.

### Eligibility and selection

Eligibility is evaluated after invalidation, when the rolled-back chain UTXOs and the
transactions returned to the mempool are both visible. A transaction qualifies only
when the reorg wallet can sign a same-input conflict, all its inputs exist on the
rolled-back chain, and it is a root rather than a descendant of another orphaned
transaction. Replacing an ancestor and letting its descendants become invalid models
the permanent drop correctly; rebuilding those descendants independently would not.

The percentage applies to eligible roots, not to every transaction in the orphaned
blocks. For a non-zero percentage and at least one eligible root, the selected count is
`max(1, floor(eligible * percentage / 100))`. Selection is reproducible: roots are
ordered by oldest orphaned block first and then by their original transaction order.
Eligibility is based on whether the configured wallet can sign, not on whether a
transaction was semantically created as spam.

Only transactions whose keys are available to the reorg wallet qualify. The default
raw spam engine (`USE_RAW_TX_SPAM=true`) signs with keys that wallet does not hold, so
stock settings usually produce zero eligible transactions. Set
`USE_RAW_TX_SPAM=false` to use wallet-engine spam. If the percentage is above zero,
raw spam is enabled, and the orphaned window has zero eligible transactions, the tool
emits a highlighted warning explaining the mismatch. Transactions signed by external
user keys are likewise ineligible; their conflict must be supplied separately.

### Why conflicts are mined directly

The conflicting transactions are placed directly in the replacement blocks instead of
being broadcast to the mempool first. An original transaction may not opt in to RBF,
and mempool replacement policy is deliberately narrower than block consensus rules. A
miner can still include a valid conflict in the winning chain, which is the behavior
this feature is intended to simulate.

Each selected root is replaced by a transaction that spends the same inputs to a fresh
wallet destination while preserving the original fee. Its output graph intentionally
changes: the original and any transactions that depended on its outputs are excluded
from the winning branch.

### Observable result and boundaries

After a successful run, every logged replacement txid is confirmed on the winning
branch; its selected original is absent from both the active chain and mempool, and its
dependent descendants are gone. A zero-eligible result does not fail the reorg: the
ordinary replacement chain is still mined and the reason is logged.

`REORG_DOUBLE_SPEND_PCT` is ignored in `empty` mode, because that mode mines no
transactions. Transactions created through `REORG_ADDS_NEW_TXS` are injected after
invalidation and therefore are not double-spend candidates during that same run.
Witness-based chain adoption and mining-controller behavior are unaffected.

## Continuous Reorgs

Reorg every `AUTO_REORG_EVERY_BLOCKS` (x) blocks, reorg `REORG_DEPTH` (y) blocks, with x > y enforced:

```bash
REORG_MODE=auto docker compose --profile reorg up btc-simnet-reorg
```

Tune `REORG_DEPTH`, `AUTO_REORG_EVERY_BLOCKS`, `REORG_NODE`, `REORG_MINE_ADDRESS`, `REORG_ADDS_NEW_TXS`, `REORG_DOUBLE_SPEND_PCT`, `REORG_WALLET_NAME` and `REORG_WITNESS_NODE` in `.env` (see [SETTINGS.md](./SETTINGS.md)).

## Safety & Mining Controller Integration

The reorg is race-safe against the mining controller: after mining the replacements the tool polls a witness node (`REORG_WITNESS_NODE`, default node1) and, if the miners kept extending the old chain in the meantime, mines extra blocks until the network adopts the new chain.

The mining controller observes reorgs like a real miner would: it keeps mining on whatever tip its node reports (so it follows the winning chain automatically) while remembering the recent chain and which blocks it mined itself. When history is rewritten it logs a `REORG detected` line with the fork point, the replaced range and the new tip (the same shape chainwatch reports), and every block it did not mine itself -- the reorg replacements, or anything generated outside the controller -- is flagged with an `EXTERNAL block` line, which also explains any height jumps in its log.
