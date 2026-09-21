# Aquarius XLM/USDC constant-product fixture

This directory contains the frozen Protocol 27 `ledger.json`/`manifest.json`
compatibility fixture, its legacy `capture-p27.json` bundle, and the current
Protocol 28 `capture.json` and `auto-capture.json` execution-driven caches.
Local WASM artifacts generate client ABIs only; the normal test suite never
contacts a network.

The scenario's primary contract is Aquarius pool
`CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE`. Executing its real
mainnet WASM exposed a write-time dependency on pool plane
`CCABO2IQYDWRGGQ4DYQ73CV3ZFDBRZTEQNDDJMFT7JZO54CLS4RYJROY`; that dependency is
therefore frozen too. The snapshot utility discovered the plane's current WASM
hash from its instance before each capture, then verified the final instance
still referenced the same code. Automatic Host-driven discovery belongs to the
capture-bundle path documented below.

## Historical frozen state

- Mainnet ledger: `63599433`
- Ledger hash: `3bdfb799014cb4d0efe0b2b8e53ef2664a805f704046f485f903472b2a94c4ed`
- Protocol: `27`
- Canonical ledger digest: `cf3ca3247927da7c7ade18a734acf416f87c9ad1509a8fa3a003a19d7c0d9b9d`
- `ledger.json` SHA-256: `e192e296547527922fc2203bcb6291c599ffb89c6dbc3584eac5e9ea1b5b57bd`
- `pool.wasm` SHA-256: `ae0da5a84b15805c5c7931ac567a8d1b34be3f26b483993d9ff80cb2c3de9852`

`ledger.json` contains ten Host-supported entries: pool instance/code, plane
instance/code/pool data, both SAC instances, both pool SAC balances, and the
Circle USDC issuer account. Each contract entry retains its live mainnet TTL.

The State Archival `ConfigSetting` is fetched in the same final batch and
recorded in `manifest.json`, but is intentionally not inserted into
`ledger.json`: Soroban Host snapshot storage accepts Account, Trustline,
ContractData, and ContractCode entries only. Its min/max TTL values instead
populate the snapshot's ledger metadata.

## Frozen snapshot coherence and provenance

The committed frozen snapshot was produced by the scenario-specific capture
path, which:

1. verifies the mainnet passphrase and exact protocol 27;
2. discovers the plane code hash from its current instance;
3. fetches a candidate latest ledger header;
4. fetches all eleven required keys in one `getLedgerEntries` call;
5. accepts the capture only when that batch reports the same ledger as the
   header, otherwise retries;
6. verifies header sequence/close time/hash, pool and plane code links, the
   pinned pool WASM bytes, SAC executables, and issuer entry before writing.

It only performs read-only JSON-RPC calls. It never builds, signs, submits, or
sends a transaction.

The test records authorization only for the local USDC mint because the live
USDC admin is a contract address: SDK `MockAuth` would otherwise register its
test auth contract at that address and replace imported state. The recorded
admin/mint tree is asserted exactly and recording is cleared. The subsequent
user swap uses an explicit exact `MockAuth` tree rooted at `pool.swap`, with the
nested `USDC.transfer` invocation. No production signature or secret is used.

## Execution-driven capture and replay

The current capture tool starts with an RPC URL. The scenario names the pool
like every other address. During execution Kanatoko automatically discovers
every Host-read or Host-written ledger key, follows each discovered contract
instance to its referenced WASM, then rematerializes the whole set at one
coherent ledger. Confirmed-absent keys are retained separately for strict
unknown-key rejection.

New captures atomically write a rootless schema-v2 bundle containing the ledger
snapshot, Present/Absent inventory, sanitized source origin, provenance, and
canonical digests. The tool immediately loads that file and replays quote ->
mint -> swap -> requote without RPC access. `pool.wasm` supplies only the
compile-time ABI; executable network WASM comes from captured `ContractCode`
entries.

The committed `capture.json` is a rootless schema-v2 Protocol 28 bundle. It was
captured read-only from `https://mainnet.sorobanrpc.com` at mainnet ledger
`64542759` (hash
`f4bc0672191ed75576d7c170726382f3b89372dfece92665bf10b7455cb00034`).
It reached a fixed point in two rounds with 12 present and six RPC-confirmed
absent entries, and recorded zero RPC reads during final replay. Host-driven
discovery found the pool plane, both SACs, the USDC issuer account, and the
USDC admin contract plus all three referenced WASM entries without supplying
those dependency IDs to the tool.

- Canonical ledger digest: `a1bcc3ff56afa730cea0ebbf748c0b23d4b95bc62c5248becfb2719d4232de00`
- Inventory digest: `9e2ec9771fd0fdc711b828b9305c3d7cfe00676f41a7f31635e30ed6c8b05347`
- Canonical bundle digest: `26ae4f8c1a880671408ef6ffd2cae4628d43611e463b5b0c70ee69ba91ee8d90`
- `capture.json` SHA-256: `6be663eb9d010f6c3acf49dadeb8b2edee0e1f73e0c2aa93e4a894eb4afc2ceb`

The preceding schema-v1 Protocol 27 bundle remains byte-for-byte preserved as
`capture-p27.json` (SHA-256
`6e75474f2583e0f44bf4f962cfd1b1436d7927fc1e357f6f1d97c797e20eb6c1`).
It is loaded only by the cross-protocol rejection test.

Capture from `https://mainnet.sorobanrpc.com` with:

```sh
cargo run --locked --features capture --bin kanatoko -- \
  capture aquarius-cp
```

Replay an existing bundle fully offline with:

```sh
cargo run --locked --offline --features capture --bin kanatoko -- \
  run aquarius-cp --format text
```

## One-scenario automatic capture

`tests/auto_runner.rs` contains one Rust body for discovery and strict replay.
It mixes a generated pool client with dynamic SAC and pool invocations in one
`Env`, creates a deterministic local G-address, explicitly funds it, establishes
its real SAC trustline, mints it 10% of the USDC reserve, swaps through the
captured graph, and proves the 1 USDC -> XLM quote moved. The same scenario
reads a real mainnet G-account through both XLM and USDC SACs, explicitly funds
a separate local sender, then transfers one stroop to the real account's
M-address and verifies the multiplexing ID in the emitted event.

The committed `auto-capture.json` was created by the runner itself on the
first online execution; no separate capture scenario or manifest was written.

- Bundle schema: `2` (no root address)
- Mainnet ledger: `64542793`
- Ledger hash:
  `0903cf35148ea360b9505af96bc09beec7eaf355fe403224cc246b5112c12006`
- Protocol: `28`
- Discovery: 2 rounds, 14 present, 4 confirmed absent
- Final replay RPC reads: `0`
- Canonical ledger digest:
  `28c83efcc17fd37785c73aa72f37ff4b1909167038814e82c0b121b798ff0a91`
- Inventory digest:
  `dca2d1fdce95a1174691fa3fdb93cd7c9e3c47d647c7806b4a67f81330e29ae7`
- Canonical bundle digest:
  `df20befe0434233ef37ac7c087e04a038752f6f8b1f4a08d503e44b1069a19af`
- `auto-capture.json` SHA-256:
  `b320b0a61603301d4fc0ccaa51466093e57b9a35a04f5b2c672b5c103e4fe68a`

The typed client is deliberately generated from the different
`kanatoko_aquarius_wrapper.wasm` artifact. The test asserts that its hash is
not the captured pool executable hash, proving the imported file is only an
ABI source and the captured network pool WASM executes. A second negative test
uses incompatible generated bindings and requires the typed `try_*` call to
return an error rather than execute the local artifact. The acceptance passes
with all HTTP proxies pointed at
`127.0.0.1:9`.

## Strict mutable candidate workflow

The strict fork loads the schema-v2 Protocol 28 capture without collapsing
Unknown into confirmed Absent. It locally injects the committed hash-pinned Aquarius wrapper
candidate, whose production WASM calls the captured pool WASM. The acceptance
estimates 1 USDC -> XLM, mints a synthetic user 10% of the captured USDC
reserve, previews and exact-gates a wrapper swap, then proves the quote
decreased from `48281703` to `39907678` in the mutated session.

Checkpoint/revert restores the first quote (`48281703`), an uncaptured contract
key fails closed after the mutations and after revert, and every receipt plus
the fork reports zero upstream reads. JSON output exposes detached XDR for
results, exact auth trees, events, diagnostics, and ledger diffs.

The observed synthetic-user authorization tree is rooted at the local wrapper
`swap`, continues through the captured pool `swap`, and ends at captured USDC
SAC `transfer`. Record mode mock-satisfies discovery; mock-exact runs in an
isolated recording child and commits only on byte-for-byte detached tree
equality. This is not signature or transaction-faithful deployment evidence.
Host-generated recording nonces are treated as mocked-auth scaffolding and are
not committed. The nonce exception is not active in enforce mode, where an
uncaptured anti-replay key remains Unknown and fails closed.

Historical frozen Protocol 27 snapshot toolchain:

- `stellar 27.0.0` (`5a7c5fe76530bf4248477ac812fc757146b98cc4`)
- `stellar-xdr 27.0.0` (`5262803470be965e42f80023d12fba12808c774a`)
- `rustc 1.94.0-nightly (e29fcf45e 2026-01-04)`
- `cargo 1.94.0-nightly (b54051b15 2025-12-30)`

The historical `ledger.json`, `manifest.json`, `pool.wasm`, and
`capture-p27.json` files remain unchanged unless intentionally regenerated by a
separate compatibility workflow. The two Protocol 28 caches can be refreshed
with the documented read-only capture commands and ignored refresh test.
