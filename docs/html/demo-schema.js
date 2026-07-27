// Config schema fixture for the static dashboard preview.
// Captured verbatim from GET /api/v1/config/schema so the settings panel
// renders with the real keys, help text, controls and defaults.
// Regenerate by dumping that endpoint from a running control plane.
window.SIMCHAIN_DEMO_SCHEMA = {
  "boot_settings": [
    {
      "group": "spam-basics",
      "help": "Always true: the raw engine signs spam locally and bypasses the node wallets. The node-wallet engine is deprecated and no longer selectable.",
      "key": "USE_RAW_TX_SPAM",
      "note": "pinned \u00b7 read-only",
      "value": "true"
    },
    {
      "group": "spam-basics",
      "help": "The nodes' boot-time -fallbackfee (BTC/kvB): the wallet feerate when fee estimation has no data. Fixed until the node containers are recreated; the live spam fee is SPAM_FEE.",
      "key": "FALLBACK_FEE",
      "note": "boot-time \u00b7 read-only",
      "value": "0.0001"
    }
  ],
  "settings": [
    {
      "apply_mode": "next_safe_point",
      "component": "mining",
      "control": "choice",
      "default": "poisson",
      "group": "mining",
      "help": "Block interval distribution: poisson (exponential, mainnet-like) or fixed (always the mean). Use poisson to test variable confirmation latency; use fixed for a predictable mining cadence.",
      "key": "BLOCK_INTERVAL_MODE",
      "optional": false,
      "options": [
        "poisson",
        "fixed"
      ]
    },
    {
      "apply_mode": "next_safe_point",
      "component": "mining",
      "control": "integer",
      "default": "15",
      "group": "mining",
      "help": "Mean seconds between blocks (positive integer). Controls simulation speed and expected confirmation latency.",
      "key": "BLOCK_INTERVAL_MEAN_SECS",
      "minimum": 1.0,
      "optional": false
    },
    {
      "apply_mode": "next_safe_point",
      "component": "mining",
      "control": "decimal",
      "default": "10",
      "group": "mining",
      "help": "Lower clamp on poisson-sampled intervals; empty = unbounded and fixed mode ignores it. Prevents unusually fast consecutive blocks in bounded demonstrations.",
      "key": "BLOCK_INTERVAL_MIN_SECS",
      "minimum": 0.0,
      "optional": true
    },
    {
      "apply_mode": "next_safe_point",
      "component": "mining",
      "control": "decimal",
      "default": "20",
      "group": "mining",
      "help": "Upper clamp on poisson-sampled intervals; empty = unbounded and fixed mode ignores it. Prevents a long poisson tail from stalling a short test run.",
      "key": "BLOCK_INTERVAL_MAX_SECS",
      "minimum": 2.220446049250313e-16,
      "optional": true
    },
    {
      "apply_mode": "next_safe_point",
      "component": "mining",
      "control": "text",
      "default": "",
      "group": "mining",
      "help": "Relative node2,node3 hashrates, e.g. 70,30; empty = strict alternation. Models unequal miner hashpower and biases which miner produces each block.",
      "key": "MINER_WEIGHTS",
      "optional": true
    },
    {
      "apply_mode": "next_safe_point",
      "component": "mining",
      "control": "integer",
      "default": "",
      "group": "mining",
      "help": "Unsigned 64-bit decimal seed for reproducible stochastic timing and miner selection. Example: 42; valid range: 0 to 18446744073709551615. Empty = generate a random seed.",
      "key": "MINING_RNG_SEED",
      "minimum": 0.0,
      "optional": true
    },
    {
      "apply_mode": "engine_rebuild",
      "component": "spam",
      "control": "toggle",
      "default": "true",
      "group": "spam-basics",
      "help": "Enable spam generation. When false the worker and raw engine remain resident, preserving branch and floor-pool state for a fast re-enable. Controls whether mined blocks carry background transaction load. Other spam settings are ignored and disabled while false.",
      "key": "ENABLE_SPAM",
      "optional": false
    },
    {
      "apply_mode": "engine_rebuild",
      "component": "spam",
      "control": "decimal",
      "default": "0.0001",
      "group": "spam-basics",
      "help": "Spam fee floor in BTC/kvB: 0.0001 = 10 sat/vB, 0.001 = 100 sat/vB, and 0.1 = 10,000 sat/vB. Floor fills pay exactly this; bulk spam pays a small premium. Higher fees also multiply the BTC needed by every raw-engine branch. For example, with 90,000-byte DATA transactions, 0.1 needs about 144 BTC per branch and about 8,650 BTC per miner at the ratio-4 auto-fanout target. An unaffordable combination of fee, payload size, and fanout causes capacity_degraded: provisioning keeps trying but cannot recover until demand is reduced or more mature funds are available. It can also drain spendable miner treasuries below the faucet reserve, making faucet capacity zero until funds recover or mined fees mature. Applies in place at a safe transaction boundary while preserving tracked funds.",
      "key": "SPAM_FEE",
      "minimum": 0.0,
      "optional": false
    },
    {
      "apply_mode": "next_safe_point",
      "component": "spam",
      "control": "decimal",
      "default": "2.0",
      "group": "spam-basics",
      "help": "DATA/HYBRID fill target in blocks of mempool weight: 0.5 = half-full blocks, 2 = full + backlog. Controls block fullness and how much pending traffic remains visible after a block. An increase triggers one immediate mempool-deficit catch-up without resetting the engine.",
      "key": "SPAM_FILL_BLOCK_RATIO",
      "minimum": 0.0,
      "optional": false
    },
    {
      "apply_mode": "engine_rebuild",
      "component": "spam",
      "control": "integer",
      "default": "90000",
      "group": "spam-basics",
      "help": "Biggest OP_RETURN payload for DATA/HYBRID fill; 0 switches to the legacy OUTPUT mode. Larger payloads fill blocks with fewer transactions without growing the spendable UTXO set.",
      "key": "SPAM_TX_DATA_MAX_BYTES",
      "maximum": 98000.0,
      "minimum": 0.0,
      "optional": false
    },
    {
      "apply_mode": "engine_rebuild",
      "component": "spam",
      "control": "integer",
      "default": "250",
      "group": "spam-basics",
      "help": "Smallest OP_RETURN payload; sizes spread log-uniformly between min and max. Controls visible transaction-size diversity; a lower minimum produces more small transactions.",
      "key": "SPAM_TX_DATA_MIN_BYTES",
      "minimum": 0.0,
      "optional": false
    },
    {
      "apply_mode": "next_safe_point",
      "component": "spam",
      "control": "integer",
      "default": "0",
      "group": "spam-advanced",
      "help": "Extra minimum-size floor-priced txs per block on top of the data fill; 0 = none. Controls how realistic the transaction-size mixture looks.",
      "key": "SPAM_SMALL_TXS_PER_BLOCK",
      "minimum": 0.0,
      "optional": false
    },
    {
      "apply_mode": "next_safe_point",
      "component": "spam",
      "control": "integer",
      "default": "4000",
      "group": "spam-advanced",
      "help": "Standing floor-priced ~110-vB self-transfers kept in the mempool (airtight fee floor); 0 = off. When blocks are full, prevents cheap transactions from slipping through residual gaps.",
      "key": "SPAM_FLOOR_POOL_TXS",
      "minimum": 0.0,
      "optional": false
    },
    {
      "apply_mode": "next_safe_point",
      "component": "spam",
      "control": "integer",
      "default": "100",
      "group": "spam-advanced",
      "help": "Fixed tx count for OUTPUT modes and the wallet engine; ignored in DATA/HYBRID mode. Controls visible transaction count and node workload when using OUTPUT mode.",
      "key": "SPAM_FIXED_TXS_PER_BLOCK",
      "minimum": 0.0,
      "optional": false
    },
    {
      "apply_mode": "engine_rebuild",
      "component": "spam",
      "control": "integer",
      "default": "0",
      "group": "spam-advanced",
      "help": "OUTPUT-mode fatness: 0 = sequential txs, N = batches of N burn outputs per tx. Higher values model payout batches and fill block weight with fewer transaction IDs, at greater UTXO cost.",
      "key": "SPAM_SENDMANY_OUTPUTS",
      "minimum": 0.0,
      "optional": false
    },
    {
      "apply_mode": "next_safe_point",
      "component": "spam",
      "control": "toggle",
      "default": "true",
      "group": "spam-advanced",
      "help": "Auto-size the branch pool from the fill ratio; false = use SPAM_FANOUT_UTXOS. The minimum is ratio x10 and the preferred target is ratio x15, so existing headroom stays active while extra branches are provisioned in the background.",
      "key": "SPAM_FANOUT_AUTO",
      "optional": false
    },
    {
      "apply_mode": "next_safe_point",
      "component": "spam",
      "control": "integer",
      "default": "50",
      "group": "spam-advanced",
      "help": "Manual preferred branch-pool size; must cover the fill ratio (>= ratio x10, min 12) when auto is off. Independent branches bypass unconfirmed-chain limits; usable branches keep sending while added capacity confirms in the background.",
      "key": "SPAM_FANOUT_UTXOS",
      "minimum": 0.0,
      "optional": false
    },
    {
      "apply_mode": "next_safe_point",
      "component": "spam",
      "control": "toggle",
      "default": "false",
      "group": "spam-advanced",
      "help": "Fee-bump (RBF) a fraction of the just-sent spam so the mempool carries real BIP125 replacements. Exercises replacement handling in explorers, wallets, and transaction monitors.",
      "key": "ENABLE_SPAM_REPLACES",
      "optional": false
    },
    {
      "apply_mode": "next_safe_point",
      "component": "spam",
      "control": "integer",
      "default": "5",
      "group": "spam-advanced",
      "help": "How many of each miner's spam txs get fee-bumped per block when RBF traffic is enabled. Controls replacement-event density and downstream processing load. Ignored while ENABLE_SPAM_REPLACES=false.",
      "key": "SPAM_REPLACES_PER_MINER_PER_BLOCK",
      "minimum": 0.0,
      "optional": false
    }
  ]
};
