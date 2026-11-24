# Stateful fixture contract

This tiny contract is source backing for Kanatoko's production-WASM fixture
runtime tests. Normal `cargo test` runs consume the committed artifact and
never rebuild it.

From the repository root, build and copy the optimized artifact with:

```sh
RUSTUP_TOOLCHAIN=1.92.0 \
RUSTC_WRAPPER=sccache \
CARGO_TARGET_DIR=target/fixture-contracts \
stellar contract build \
  --manifest-path fixtures/contracts/stateful/Cargo.toml \
  --out-dir fixtures/wasm \
  --optimize \
  --locked
```

Artifact: `fixtures/wasm/kanatoko_stateful_fixture.wasm`

SHA-256: `76a511a1e7a1fff4a5fecb77f6d6a4018c9f090ff40a8a52b7233c44d71aa18e`

The committed artifact is 8,091 bytes and was built from `soroban-sdk 30.0.0`
(`414dd7dd95e6798db7d23899f94187017e41077d`) with Stellar CLI 30.0.1
(`f2a7b74175c34585f3b30a1abf42a2d41cbcc390`) and rustc 1.92.0 for
`wasm32v1-none`, with `RUSTC_WRAPPER=sccache`.
