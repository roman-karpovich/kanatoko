# Aquarius XLM/USDC constant-product fixture

This directory contains the frozen `ledger.json`/`manifest.json` compatibility
fixture, the lower-level `capture.json` bundle, and the one-scenario
`auto-capture.json` cache. Local WASM artifacts generate client ABIs only; the
normal test suite never contacts a network.

The root contract is Aquarius pool
`CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE`. Executing its real
mainnet WASM exposed a write-time dependency on pool plane
`CCABO2IQYDWRGGQ4DYQ73CV3ZFDBRZTEQNDDJMFT7JZO54CLS4RYJROY`; that dependency is
therefore frozen too. The snapshot utility discovered the plane's current WASM
hash from its instance before each capture, then verified the final instance
still referenced the same code. Automatic Host-driven discovery belongs to the
capture-bundle path documented below.

## Captured state

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

## Address-first capture and replay

The current capture tool starts with the pool address and RPC URL. During
scenario execution it automatically discovers every Host-read or Host-written
ledger key, follows each discovered contract instance to its referenced WASM,
then rematerializes the whole set at one coherent ledger. Confirmed-absent keys
are retained separately for strict unknown-key rejection.

It atomically writes one self-validating `capture.json` containing the ledger
snapshot, Present/Absent inventory, root address, sanitized source origin,
provenance, and canonical digests. It immediately loads that file and replays
quote -> mint -> swap -> requote without RPC access. `pool.wasm` supplies only
the compile-time ABI; executable network WASM comes from captured
`ContractCode` entries.

The committed capture bundle was captured at mainnet ledger `63600296` (hash
`63aa87f14ca20f1761fd5b055359eb864db3555a33bece46a68df8fb673ece94`).
It reached a fixed point in two rounds with 12 present and six RPC-confirmed
absent entries, and recorded zero RPC reads during final replay. Host-driven
discovery found the pool plane, both SACs, the USDC issuer account, and the
USDC admin contract plus all three referenced WASM entries without supplying
those dependency IDs to the tool.

- Canonical ledger digest: `eb0e7c7805f62c8362ee8a46de6dd89bc9a6568e8c490eab96ea70fbc8d19824`
- Inventory digest: `e765558e1a8bb31adf20084862984c2e5d5b9f0c0d5d20fc30197b6a0dd062f3`
- Canonical bundle digest: `07163128f427a183a7e6563cdcaf0796019b458da6e6a3ca8c572a6b5f8aa9d2`
- `capture.json` SHA-256: `6e75474f2583e0f44bf4f962cfd1b1436d7927fc1e357f6f1d97c797e20eb6c1`

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
`Env`, mints a synthetic user 10% of the USDC reserve, swaps through the real
captured graph, and proves the 1 USDC -> XLM quote moved.

The committed `auto-capture.json` was created by the runner itself on the
first online execution; no separate capture scenario or manifest was written.

- Mainnet ledger: `63608632`
- Ledger hash:
  `b1602d78266433255c154a6460ed223e64d3f766b2548065125150e58899e2ee`
- Discovery: 2 rounds, 12 present, 5 confirmed absent
- Final replay RPC reads: `0`
- Canonical ledger digest:
  `3362768e3989ae0905afc62813aabb76cf99e1bd3478ea121be4c8651fac70ed`
- Inventory digest:
  `71612d82409b8a26560a9a8b3e07563c38b5a7cce83a043247c1e3d26f9e0fcd`
- Canonical bundle digest:
  `33e9edc72cc6afe4e4b55a5b647936ce3367a33e79495f97d14d4dc3fa1f6b6b`
- `auto-capture.json` SHA-256:
  `56dcd3072469ddafa9e2dbd9c3490ab373b2967bb6932419a659e0ea22860afa`

The typed client is deliberately generated from the different
`kanatoko_aquarius_wrapper.wasm` artifact. The test asserts that its hash is
not the captured root executable hash, proving the imported file is only an
ABI source and the captured network pool WASM executes. A second negative test
uses incompatible generated bindings and requires the typed `try_*` call to
return an error rather than execute the local artifact. The acceptance passes
with all HTTP proxies pointed at
`127.0.0.1:9`.

## Strict mutable candidate workflow

The strict fork loads this schema-v1 capture without collapsing Unknown into
confirmed Absent. It locally injects the committed hash-pinned Aquarius wrapper
candidate, whose production WASM calls the captured pool WASM. The acceptance
estimates 1 USDC -> XLM, mints a synthetic user 10% of the captured USDC
reserve, previews and exact-gates a wrapper swap, then proves the quote
decreased from `53354881` to `44100959` in the mutated session.

Checkpoint/revert restores the first quote (`53354881`), an uncaptured contract
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

Frozen snapshot toolchain at capture:

- `stellar 27.0.0` (`5a7c5fe76530bf4248477ac812fc757146b98cc4`)
- `stellar-xdr 27.0.0` (`5262803470be965e42f80023d12fba12808c774a`)
- `rustc 1.94.0-nightly (e29fcf45e 2026-01-04)`
- `cargo 1.94.0-nightly (b54051b15 2025-12-30)`

The frozen snapshot files remain unchanged unless intentionally regenerated by
a separate compatibility workflow.
