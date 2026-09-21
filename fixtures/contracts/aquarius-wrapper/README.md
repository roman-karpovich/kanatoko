# Aquarius wrapper fixture contract

This small contract is source backing for Kanatoko's candidate-contract
acceptance workflow. It stores the captured Aquarius pool address in its
constructor and calls the real captured pool through a minimal generated
Soroban client. No pool behavior is replaced by a native mock.

`get_tokens`, `get_reserves`, and `estimate_swap` proxy read calls. `swap`
requires Stellar-native authorization from `user` before calling the captured
pool, allowing the observed authorization tree to include wrapper, pool, and
Stellar Asset Contract invocations.

From the repository root, build and copy the optimized artifact with:

```sh
RUSTUP_TOOLCHAIN=1.92.0 \
RUSTC_WRAPPER=sccache \
CARGO_TARGET_DIR=target/fixture-contracts \
stellar contract build \
  --manifest-path fixtures/contracts/aquarius-wrapper/Cargo.toml \
  --out-dir fixtures/wasm \
  --optimize \
  --locked
```

Artifact: `fixtures/wasm/kanatoko_aquarius_wrapper.wasm`

SHA-256: `e4d626c4960bc879dd44c8243c04b890a9bb0ddb7e1392d84a5b651c15aaab4f`

The committed artifact is 4,510 bytes and was built from `soroban-sdk 28.0.0`
(`48d506712f964094d14176e2f0b02afcd1054567`) with Stellar CLI 27.0.0
(`5a7c5fe76530bf4248477ac812fc757146b98cc4`) and rustc 1.92.0 for
`wasm32v1-none`, with `RUSTC_WRAPPER=sccache`.
