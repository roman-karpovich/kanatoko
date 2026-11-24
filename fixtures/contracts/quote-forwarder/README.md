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

SHA-256: `bafe532ffb80260d56a07cec4c30b3fa512e6403067a0552266ebbbc2d1a25ba`

The committed artifact is 3,284 bytes and was built from `soroban-sdk 30.0.0`
(`414dd7dd95e6798db7d23899f94187017e41077d`) with Stellar CLI 30.0.1
(`f2a7b74175c34585f3b30a1abf42a2d41cbcc390`) and rustc 1.92.0 for
`wasm32v1-none`, with `RUSTC_WRAPPER=sccache`.
