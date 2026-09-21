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

SHA-256: `0156a9a840e4147732fcf0479846220840ae4b4281c58354aa31cf70daf6b2ea`

The committed artifact is 8,153 bytes and was built from `soroban-sdk 28.0.0`
(`48d506712f964094d14176e2f0b02afcd1054567`) with Stellar CLI 27.0.0
(`5a7c5fe76530bf4248477ac812fc757146b98cc4`) and rustc 1.92.0 for
`wasm32v1-none`, with `RUSTC_WRAPPER=sccache`.
