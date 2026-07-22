# Code review findings

Open findings from the last full code review, kept here so this is the single tracking
document. Everything the review found fixed has been dropped; the items below were
re-verified against the code on the review date.

Accepted decisions (not defects, recorded so they are not re-reported):

- **RPC bound on all host interfaces** (`-rpcallowip=0.0.0.0/0` + unrestricted port
  binding): intentional, reaching the simnet from another machine is a wanted use case.

- **Plaintext RPC credentials on the bitcoind command line** (visible in
  `docker inspect`/`ps`): acceptable for a throwaway regtest; documented with a warning
  in SETTINGS.md not to replicate in production.

No open findings from the last review remain.

---

# Limitations and future enhancements


## Simulations

- Per-node policies: give each node different bitcoind parameters (mempool size,
  relay fees, RBF policy) or even different bitcoind versions/images, like a real
  heterogeneous network (the compose file already declares each node in full to
  allow this)

### Walletless mainnet transaction fixture importer

**Status (2026-07-17): planned/design only** — full design in
[raw-transaction-fixture-importer-plan.md](raw-transaction-fixture-importer-plan.md).

Import selected mainnet transactions as Simchain-valid raw transaction fixtures without
using node wallets. The importer would fetch source transactions from a mainnet node,
preserve useful artifacts such as OP_RETURN data, witness payloads, script shape, value
layout, and fee/weight profile where possible, replace inputs with Simchain-funded UTXOs,
rewrite spendable outputs to fixture-owned regtest keys, sign raw transactions, broadcast
them, and return a manifest mapping source txids to Simchain txids plus the external
spend authority needed by user tests.

This is not a mainnet fork and cannot preserve original txids after sanitization; it is a
way to replay interesting transaction artifacts under controlled regtest funds.


# Simchain Nice-to-have Features

Simchain's purpose is to simulate the Bitcoin chain on regtest while staying as close to
mainnet reality as regtest allows, but also providing a "controlled by the user environment"
that allows to defining mining pace, block filling and fee rates.
It consists on: multiple P2P-connected nodes, rotating miners,
a non-mining full node as the user endpoint, non-empty blocks, and user-controlled
parameters (block time, tx per block, reorgs, ...). This document gathers all the known
limitations and future enhancements, and a section for parked features.

### Chaos monkey mode

## Parked features

Designed but deliberately not built. Each entry records why it is parked and what would
revive it; the expensive design thinking is preserved in `parked/`.

### Fee-market simulation in the spammer — PARKED

**Status (2026-07-10): parked** — complexity/benefit says wait for a concrete
fee-estimation or fee-bumping test need. Full design (CPFP-safe per-branch fee ladder,
funding-pull deadlock fix) in [parked/fee-market-plan.md](parked/fee-market-plan.md),
which supersedes the implementation sketch that used to live here.

**What:** Make the spammer emit transactions with varied fee rates (sampled from a
configurable distribution, e.g. log-normal between `SPAM_FEE_MIN`/`SPAM_FEE_MAX` sat/vB)
and varied sizes/output counts, instead of identical 540-sat dust sends at fallback fee.

**Why it's a nice-to-have:** With uniform transactions, `estimatesmartfee`, mempool fee
histograms (visible in the mempool explorer) and any RBF/fee-bumping logic in the project
under test are meaningless, everything sits in one fee bucket. A spread of fee rates
creates real block-space competition: when spam volume exceeds block capacity, low-fee
transactions genuinely wait, which is exactly the mainnet behavior users want to
reproduce with the "tx per block" knob. Pairs well with the shipped Poisson block timing
(bursty blocks + fee spread = realistic mempool).

---

# Tech debt

- Build from sources instead of downloading binaries

Multi-platform
- convert all bash scripts to rust compilable binaries, so its muti platform, or run the scripts inside an ephemeral containers connected with networks and volumes?
- save snapshots in a muti-platform format instead of .tar
- at snapshots save also mempool-space elctrs data? so we do not loose stale blocks caused by reorgs?
