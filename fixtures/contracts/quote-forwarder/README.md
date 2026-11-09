# Quote forwarder fixture contract

This minimal contract stores one target contract address in its constructor and
forwards `estimate_swap(in_idx, out_idx, in_amount)` to it. The testnet fixture
tests deploy it next to the stateful fixture contract to prove that a locally
deployed contract can call another locally deployed contract through a typed
client.

From the repository root, build and copy the optimized artifact with:

```sh
RUSTUP_TOOLCHAIN=1.92.0 \
RUSTC_WRAPPER=sccache \
CARGO_TARGET_DIR=target/fixture-contracts \
stellar contract build \
  --manifest-path fixtures/contracts/quote-forwarder/Cargo.toml \
  --out-dir fixtures/wasm \
  --optimize \
  --locked
```

Artifact: `fixtures/wasm/kanatoko_quote_forwarder.wasm`

SHA-256: `097956e80c94d40d2a8734f9e8db786632cb125baf14d70c7e3d2b5a5937062d`

The committed artifact is 3,321 bytes and was built from `soroban-sdk 28.0.0`
(`48d506712f964094d14176e2f0b02afcd1054567`) with Stellar CLI 27.0.0
(`5a7c5fe76530bf4248477ac812fc757146b98cc4`) and rustc 1.92.0 for
`wasm32v1-none`, with `RUSTC_WRAPPER=sccache`.
