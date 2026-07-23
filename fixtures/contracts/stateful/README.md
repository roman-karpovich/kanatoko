# Stateful fixture contract

This tiny contract is source backing for Kanatoko's production-WASM fixture
runtime tests. Normal `cargo test` runs consume the committed artifact and
never rebuild it.

From the repository root, build and copy the optimized artifact with:

```sh
stellar contract build \
  --manifest-path fixtures/contracts/stateful/Cargo.toml \
  --out-dir fixtures/wasm \
  --optimize \
  --locked
```

Artifact: `fixtures/wasm/kanatoko_stateful_fixture.wasm`

SHA-256: `6f6f469798b686cc485ad207f32e3f77009c4b69ab2437d9bdca97f149b54ba8`

The committed artifact is 2,257 bytes and was built with Stellar CLI 27.0.0
(`5a7c5fe76530bf4248477ac812fc757146b98cc4`) and rustc
1.94.0-nightly (`e29fcf45e`, 2026-01-04) for `wasm32v1-none`.
