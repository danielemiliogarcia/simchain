# Implementation plan: walletless mainnet transaction fixture importer

## Status

**PLANNED / DESIGN ONLY** — written 2026-07-17.

This is a future feature plan. It defines a Simchain module that imports selected
mainnet transactions as Simchain-valid raw transaction fixtures without relying on
Bitcoin Core node wallets. The feature is recorded from
[NICE-TO-HAVE.md](NICE-TO-HAVE.md) and is not implemented.

## 1. Decision summary

Build this as a walletless fixture importer, not as a mainnet block fork.

The importer reads one or more source transactions from a configured mainnet Bitcoin
Core RPC endpoint, extracts the parts that are useful for simulation, then constructs
new regtest-valid raw transactions funded by Simchain UTXOs and signed by fixture keys
owned by the importer. It returns a manifest that maps every source txid to the new
Simchain txid and records how each output can be spent by external user test code.

The node remains only a validator, relay endpoint, and miner participant. It must not
own the fixture keys through a wallet.

Pinned direction:

1. The user supplies source mainnet txids, not block ranges.
2. The importer fetches source raw transactions and, when needed, source prevout data.
3. The importer owns fixture key generation and raw signing.
4. Inputs are replaced with Simchain-funded UTXOs.
5. Outputs are either preserved as unspendable artifacts or rewritten to fixture-owned
   regtest keys, according to an explicit mode.
6. Transactions are signed with raw keys and broadcast through `testmempoolaccept` plus
   `sendrawtransaction`.
7. The result is a manifest, not a claim that the original transaction was replayed
   exactly.

## 2. Why this is not a mainnet fork

Bitcoin transactions commit to exact inputs. If a source transaction spends a mainnet
outpoint, replacing that input with a Simchain UTXO invalidates the original signature
and changes the transaction id. Simchain therefore cannot sanitize a mainnet transaction
and keep the same txid.

Bitcoin also does not have Ethereum-style global contract state to fork. The practical
state a test needs is a set of spendable UTXOs plus realistic chain/mempool artifacts
around them.

This feature should therefore be described as fixture generation:

- source txids are references;
- output data and transaction shape can be preserved where feasible;
- all spendable funds and signatures are newly created for Simchain;
- every generated tx has a new txid.

## 3. Goals and non-goals

### Goals

- Import a caller-supplied list of mainnet txids as Simchain-valid raw transactions.
- Avoid any dependency on node wallets for fixture keys, signing, or later spending.
- Preserve transaction artifacts that matter to user systems:
  - OP_RETURN bytes;
  - witness payloads when technically practical;
  - output script class and count;
  - input/output count;
  - approximate value layout;
  - vsize/weight profile;
  - fee-rate profile.
- Return an explicit manifest with source-to-generated mappings.
- Give external test code spend authority for rewritten spendable outputs.
- Support deterministic fixture generation for reproducible tests.
- Keep Bitcoin Core policy and consensus behavior unchanged.

### Non-goals

- No exact mainnet replay.
- No preservation of source txids after rewriting inputs or outputs.
- No import of arbitrary mainnet UTXOs into regtest consensus.
- No node-wallet dependency for signing or key export.
- No general hosted signing oracle.
- No dashboard-first key export surface.
- No mutation of Bitcoin Core network/address parameters to accept mainnet addresses on
  regtest.
- No guarantee that every exotic script can be converted into a spendable equivalent.

## 4. Trust and ownership model

The importer owns fixture keys and exposes them only through the fixture manifest.

Recommended key modes:

| Mode | Behavior |
| --- | --- |
| `ephemeral` | Generate fresh keys for one import run; manifest contains the only spend authority. |
| `deterministic` | Derive keys from a user-provided seed plus source txid/vout for reproducible CI fixtures. |
| `external_funding` | Caller provides funding UTXOs and private keys; importer signs only the generated txs. |

The control plane should never log private keys. If private keys are exported, write them
only to an explicit local file path requested by the caller and make the file owner-only
readable where the platform supports it.

## 5. Import modes

Each output from a source transaction needs an explicit policy. The importer should not
guess silently when preservation conflicts with spendability.

| Mode | Description | Spendable |
| --- | --- | --- |
| `shape_only` | Recreate input/output counts, approximate weight, and broad script classes. | yes, for rewritten outputs |
| `preserve_op_return` | Copy OP_RETURN payloads exactly and rewrite spendable value outputs. | data outputs no, rewritten outputs yes |
| `preserve_outputs_unspendable` | Copy output scripts and values where standardness permits. | normally no |
| `rewrite_spendable` | Replace selected outputs with fixture-owned regtest scripts. | yes |
| `preserve_witness_payloads` | Rebuild a new spend path carrying copied witness payload data where practical. | depends on construction |
| `reference_only` | Store source metadata without broadcasting a generated transaction. | no |

Default should be conservative:

```yaml
mode: preserve_op_return
spendable_outputs: rewrite
```

That gives users useful data artifacts while ensuring the resulting UTXOs can be spent by
their external test systems.

## 6. Fixture manifest

Every import returns a manifest. The manifest is the contract between Simchain and the
external system under test.

Example shape:

```json
{
  "version": 1,
  "source_chain": "mainnet",
  "generated_chain": "regtest",
  "imports": [
    {
      "source_txid": "mainnet-txid",
      "simchain_txid": "regtest-txid",
      "mode": "preserve_op_return",
      "raw_hex": "020000...",
      "inputs_replaced": true,
      "txid_preserved": false,
      "outputs": [
        {
          "source_vout": 0,
          "simchain_vout": 0,
          "amount_sat": 50000,
          "script_policy": "rewrite_spendable",
          "address": "bcrt1...",
          "descriptor": "wpkh(...)",
          "private_key_wif": "optional-explicit-export-only",
          "spendable": true
        },
        {
          "source_vout": 1,
          "simchain_vout": 1,
          "amount_sat": 0,
          "script_policy": "preserve_op_return",
          "payload_sha256": "...",
          "spendable": false
        }
      ],
      "warnings": [
        "source txid changed because inputs were replaced"
      ]
    }
  ]
}
```

Manifest rules:

- Always include `source_txid` and `simchain_txid` when a transaction is broadcast.
- Always indicate whether a txid was preserved. For sanitized transactions this is
  normally `false`.
- Never include private keys unless the caller explicitly asks for key export.
- Include descriptors or scripts for every spendable generated output.
- Include warnings for lossy transformations.

## 7. Funding model

The importer needs Simchain UTXOs to replace source inputs.

Supported funding options, in preferred order:

1. **Fixture funding transaction**: use an existing Simchain faucet or raw funding helper
   to create confirmed UTXOs for importer-owned keys.
2. **Caller-provided UTXOs**: external tests provide outpoints, amounts, scripts, and
   signing keys.
3. **Pre-generated fixture pool**: a future setup step maintains many confirmed fixture
   UTXOs for fast imports.

Coinbase/miner wallets may fund the initial fixture pool through existing Simchain tools,
but imported transactions themselves must be built and signed walletlessly.

## 8. Raw transaction construction

Implementation should live in Rust, likely split between a new crate and
`simchain-common` helpers.

Expected responsibilities:

- fetch source transaction and optional prevout metadata from mainnet RPC;
- classify source scripts and outputs;
- choose import policy per output;
- select or request Simchain funding UTXOs;
- construct replacement inputs;
- construct preserved or rewritten outputs;
- estimate and pad weight where `shape_only` requires approximate size matching;
- sign with fixture private keys;
- call `testmempoolaccept`;
- broadcast with `sendrawtransaction`;
- optionally wait for confirmation through existing scenario `wait_tx` support.

For signing, use an explicit raw-key path. Bitcoin Core's `signrawtransactionwithkey`
is acceptable for a first implementation if all keys and prevout metadata are supplied
directly, because it does not require wallet ownership. A native Rust signer is cleaner
long term if the project already has or gains shared signing code.

## 9. CLI and scenario surface

Start CLI-first. The dashboard can link to results later, but it should not be the first
place raw keys appear.

Possible CLI:

```bash
simchainctl fixtures import-tx \
  --source-rpc-url "$MAINNET_RPC_URL" \
  --source-rpc-user "$MAINNET_RPC_USER" \
  --source-rpc-pass "$MAINNET_RPC_PASS" \
  --txid "$TXID" \
  --mode preserve-op-return \
  --manifest ./fixture-manifest.json \
  --export-private-keys ./fixture-keys.json
```

Possible scenario step:

```yaml
- type: import_tx_fixture
  txids:
    - "..."
  mode: preserve_op_return
  key_seed_env: FIXTURE_KEY_SEED
  manifest_path: ./artifacts/fixture-manifest.json
  wait_confirmed: true
```

The scenario form should come after the importer is stable as a direct CLI/API feature.

## 10. API and storage

The control plane can eventually expose a job API, but the first version may be a local
CLI command that talks to both mainnet RPC and Simchain RPC.

If implemented in the control plane later:

- store manifests as job artifacts, not as durable chain policy;
- redact key material from normal job events;
- require explicit key export flags;
- do not put source mainnet RPC credentials in durable control-plane state;
- keep one mutation coordinator if the job funds or broadcasts Simchain transactions.

## 11. Validation and safety

Validation should reject:

- non-mainnet source txids when source chain validation is available;
- source transactions with unsupported scripts unless mode is `reference_only`;
- attempts to preserve mainnet addresses as spendable regtest destinations;
- key export paths that are missing when the caller asks for external spend authority
  outside the manifest;
- insufficient Simchain funding UTXOs;
- transactions rejected by `testmempoolaccept`.

The importer must be explicit about lossy conversions. A successful import with warnings
is acceptable; a silent downgrade is not.

## 12. Implementation phases

### Phase 1 — Offline analyzer

- Add a crate or command that fetches source txids from mainnet RPC.
- Classify transaction shape, script types, OP_RETURN data, witness sizes, and fees.
- Emit a reference-only manifest.

### Phase 2 — Walletless generated transactions

- Generate fixture keys.
- Fund fixture keys from caller-provided Simchain UTXOs.
- Build and sign simple P2WPKH replacement transactions.
- Broadcast and return source-to-generated mappings.

### Phase 3 — Artifact preservation

- Preserve OP_RETURN payloads.
- Add output rewrite policies.
- Add approximate weight/value profile matching.
- Add deterministic key seed support.

### Phase 4 — External spend workflow

- Export descriptors/WIFs by explicit request.
- Add examples showing an external program spending generated outputs using only the
  manifest.
- Add `wait_tx` scenario integration examples.

### Phase 5 — Scenario/API integration

- Add an optional scenario step.
- Add control-plane job support only if there is a concrete need for server-side import
  orchestration.

## 13. Acceptance tests

- Import a mainnet tx containing OP_RETURN and verify the Simchain tx preserves the
  payload bytes.
- Import a tx in `rewrite_spendable` mode and spend the generated output using only
  manifest-provided keys/descriptors.
- Confirm source txid and Simchain txid are different and documented in the manifest.
- Confirm no Bitcoin Core wallet owns the fixture output unless explicitly imported by
  the user after the fact.
- Confirm private keys are absent from ordinary logs and job events.
- Confirm `testmempoolaccept` failure produces a clear error and no misleading manifest.

## 14. Open questions

- Should the first implementation use native Rust signing or
  `signrawtransactionwithkey`?
- Should funding be supplied only by caller UTXOs at first, or should the faucet grow a
  raw fixture funding mode?
- How much witness payload preservation is worth building before a concrete ordinals or
  script-heavy test case appears?
- Should manifests be stored only as local files, or also as control-plane artifacts once
  API integration exists?
