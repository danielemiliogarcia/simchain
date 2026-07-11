# Simulating Reorgs

The reorg simulator (a Rust container using only bitcoind RPC calls) invalidates the last *N* blocks on a miner node and mines *N+1* replacements, so the new chain is strictly longer and **the whole network reorgs to it**. Transactions from the orphaned blocks fall back to the mempool; each replacement block is filled by re-reading the mempool live and mining a slice of it with `generateblock`, like the winning chain of a real reorg, so reorged blocks are not empty. Reading the mempool fresh for each block means an RBF replacement that evicts an orphaned tx mid-reorg (e.g. with `ENABLE_SPAM_REPLACES=true`) is picked up automatically. On top of the returned txs it seeds `REORG_ADDS_NEW_TXS` fresh wallet transactions into the mempool first, modelling a node that received transactions its peers have not yet seen. It prints each block's hash and tx count before/after plus a replaced-blocks summary.

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

It logs the configured percentage, the eligible/selected counts, and every `old_txid -> new_txid` pair (with how many descendants each replacement pruned), so the drop is auditable. Details of the semantics:

- **Only the reorg node's own wallet spam is eligible.** The conflict is signed with the reorg node's wallet (`REORG_WALLET_NAME`), so only txs it can re-sign qualify. The default spam engine is the *raw* engine (`USE_RAW_TX_SPAM=true`), whose txs are signed by keys the reorg node does not hold, so **with stock settings there are usually zero eligible txs** and the reorg runs normally with a "0 eligible" log line. Set `USE_RAW_TX_SPAM=false` (wallet engine) to produce eligible txs. User-owned external-key txs are also never eligible; to drop one of those, broadcast the conflict yourself after an `empty` reorg.
- **Root txs only.** A tx that spends the output of another orphaned tx is a descendant, not a root; the tool double-spends the ancestor and lets the descendant die with it (descendants are excluded from the replacement blocks, never mined).
- **Ignored in `empty` mode** (empty means empty), with an explicit log line.
- **Deterministic:** eligible txs are selected oldest-orphaned-block first, and `1`–`100` always selects at least one.

## Continuous Reorgs

Reorg every `AUTO_REORG_EVERY_BLOCKS` (x) blocks, reorg `REORG_DEPTH` (y) blocks, with x > y enforced:

```bash
REORG_MODE=auto docker compose --profile reorg up btc-simnet-reorg
```

Tune `REORG_DEPTH`, `AUTO_REORG_EVERY_BLOCKS`, `REORG_NODE`, `REORG_MINE_ADDRESS`, `REORG_ADDS_NEW_TXS`, `REORG_DOUBLE_SPEND_PCT`, `REORG_WALLET_NAME` and `REORG_WITNESS_NODE` in `.env` (see [SETTINGS.md](./SETTINGS.md)).

## Safety & Mining Controller Integration

The reorg is race-safe against the mining controller: after mining the replacements the tool polls a witness node (`REORG_WITNESS_NODE`, default node1) and, if the miners kept extending the old chain in the meantime, mines extra blocks until the network adopts the new chain.

The mining controller observes reorgs like a real miner would: it keeps mining on whatever tip its node reports (so it follows the winning chain automatically) while remembering the recent chain and which blocks it mined itself. When history is rewritten it logs a `REORG detected` line with the fork point, the replaced range and the new tip (the same shape chainwatch reports), and every block it did not mine itself -- the reorg replacements, or anything generated outside the controller -- is flagged with an `EXTERNAL block` line, which also explains any height jumps in its log.
