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
stellar contract build \
  --manifest-path fixtures/contracts/aquarius-wrapper/Cargo.toml \
  --out-dir fixtures/wasm \
  --optimize \
  --locked
```

Artifact: `fixtures/wasm/kanatoko_aquarius_wrapper.wasm`

SHA-256: `798c959e1e22093c49b4ec6636aafed14e889614fb243426abe5023b30c17520`

The committed artifact is 4,404 bytes and was built with Stellar CLI 27.0.0
(`5a7c5fe76530bf4248477ac812fc757146b98cc4`) and rustc
1.94.0-nightly (`e29fcf45e`, 2026-01-04) for `wasm32v1-none`.
