# Aquarius XLM/USDC constant-product fixture

This directory contains the frozen Protocol 27 `ledger.json`/`manifest.json`
compatibility fixture, its legacy `capture-p27.json` bundle, the preceding
Protocol 28 `capture-p28.json` bundle, and the current Protocol 29
`capture.json` and `auto-capture.json` execution-driven caches.
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

The committed `capture.json` is a rootless schema-v2 Protocol 29 bundle. It was
captured read-only from `https://mainnet.sorobanrpc.com` at mainnet ledger
`64731471` (hash
`1a8181f06788ff2db2d2099b487376c82d580e39691105abef2822154a0a023d`).
It reached a fixed point in two rounds with 12 present and six RPC-confirmed
absent entries, and recorded zero RPC reads during final replay. Host-driven
discovery found the pool plane, both SACs, the USDC issuer account, and the
USDC admin contract plus all three referenced WASM entries without supplying
those dependency IDs to the tool.

- Canonical ledger digest: `7ff8b3606ec09f7c8c2937bcf493928dadbba455d1a8fcf78da9bc6d174d8c6d`
- Inventory digest: `f669b78b2823ac59874e204e00bf979252ba5a8db5c8894bd5329d5273ad9ef4`
- Canonical bundle digest: `a1f6cd183948092ee5cbe4e61b071f74e40e8d39084d5c7f166dbf79c3f16292`
- `capture.json` SHA-256: `8a4546aa123fb20ffb188c8c4c732a6780ad19be78f03453803462c68523253f`

The preceding schema-v2 Protocol 28 bundle (mainnet ledger `64542759`) remains
byte-for-byte preserved as `capture-p28.json` (SHA-256
`6be663eb9d010f6c3acf49dadeb8b2edee0e1f73e0c2aa93e4a894eb4afc2ceb`), and the
schema-v1 Protocol 27 bundle as `capture-p27.json` (SHA-256
`6e75474f2583e0f44bf4f962cfd1b1436d7927fc1e357f6f1d97c797e20eb6c1`).
They are loaded only by the cross-protocol rejection tests.

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
- Mainnet ledger: `64731476`
- Ledger hash:
  `34959e05803f638574875901ffb119fd647ea4c00f1c15b8eef3c6077cc79d87`
- Protocol: `29`
- Discovery: 2 rounds, 14 present, 4 confirmed absent
- Final replay RPC reads: `0`
- Canonical ledger digest:
  `5d736aab69d77c3b6e5737cc2b38f81399fead7cd976223183e8a9a950c39f86`
- Inventory digest:
  `608239a0904a77a30c2b9e3c9e46e3a0007e941d3ee6d23a0f3b41287cba44bb`
- Canonical bundle digest:
  `f76eba07497de8ccf6f8fc7aa2acb8bde73f5352fea4c9d9a0a17c306aa58d2b`
- `auto-capture.json` SHA-256:
  `fa35dc3c495ebbe3899a5bafee3c0ca908708d1d1df8974eee6b144fcfe6b0aa`

Refresh it with the ignored read-only test:

```sh
cargo test --locked --all-features --test auto_runner \
  refresh_protocol_29_mainnet_fixture -- --exact --ignored
```

The typed client is deliberately generated from the different
`kanatoko_aquarius_wrapper.wasm` artifact. The test asserts that its hash is
not the captured pool executable hash, proving the imported file is only an
ABI source and the captured network pool WASM executes. A second negative test
uses incompatible generated bindings and requires the typed `try_*` call to
return an error rather than execute the local artifact. The acceptance passes
with all HTTP proxies pointed at
`127.0.0.1:9`.

## Strict mutable candidate workflow

The strict fork loads the schema-v2 Protocol 29 capture without collapsing
Unknown into confirmed Absent. It locally injects the committed hash-pinned Aquarius wrapper
candidate, whose production WASM calls the captured pool WASM. The acceptance
estimates 1 USDC -> XLM, mints a synthetic user 10% of the captured USDC
reserve, previews and exact-gates a wrapper swap, then proves the quote
decreased from `44629339` to `36888783` in the mutated session.

Checkpoint/revert restores the first quote (`44629339`), an uncaptured contract
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

The historical `ledger.json`, `manifest.json`, `pool.wasm`, `capture-p27.json`,
and `capture-p28.json` files remain unchanged unless intentionally regenerated
by a separate compatibility workflow. The two Protocol 29 caches can be
refreshed with the documented read-only capture commands and ignored refresh
test.
