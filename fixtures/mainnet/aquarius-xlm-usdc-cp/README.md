# Aquarius XLM/USDC constant-product fixture

This is a frozen, fully offline fixture for one Kanatoko M1 scenario. It is not
a general RPC-backed fork and the normal test suite never contacts a network.

The root contract is Aquarius pool
`CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE`. Executing its real
mainnet WASM exposed a write-time dependency on pool plane
`CCABO2IQYDWRGGQ4DYQ73CV3ZFDBRZTEQNDDJMFT7JZO54CLS4RYJROY`; that dependency is
therefore frozen too. The capture utility discovers the plane's current WASM
hash from its instance before each capture, then verifies the final instance
still references the same code. Automatically discovering every Host-touched
dependency from only a root contract ID is a later Kanatoko capability, not an
M1 claim.

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

## Coherence and provenance

The scenario-specific capture utility:

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

Captured from `https://mainnet.sorobanrpc.com` with:

```sh
cargo run --locked --features capture --bin capture-aquarius-xlm-usdc-cp
```

Toolchain at capture:

- `stellar 27.0.0` (`5a7c5fe76530bf4248477ac812fc757146b98cc4`)
- `stellar-xdr 27.0.0` (`5262803470be965e42f80023d12fba12808c774a`)
- `rustc 1.94.0-nightly (e29fcf45e 2026-01-04)`
- `cargo 1.94.0-nightly (b54051b15 2025-12-30)`

After recapturing, review `manifest.json` and intentionally update the fixture
digest and ledger sequence pinned in `tests/m1_aquarius.rs`.
