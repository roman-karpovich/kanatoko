# Soroswap wrapper fixture contract

This small contract is source backing for Kanatoko's candidate-contract
acceptance workflow. It stores the captured Soroswap router and XLM/USDC pair
addresses in its constructor and calls the real captured contracts through
minimal generated Soroban clients. No pool behavior is replaced by a native
mock.

`token_0`, `token_1`, and `get_reserves` proxy read calls to the pair, and
`router_get_amounts_out` proxies a read call to the router.
`swap_exact_tokens_for_tokens` requires Stellar-native authorization from `to`
before calling the router, so the observed authorization tree includes wrapper,
router, and Stellar Asset Contract invocations.

The functions deliberately mirror the matching pair and router methods, so the
artifact also serves the test suite as an ABI source for generated clients: a
client generated from it can target the captured pair or router address while
the captured network WASM executes.

From the repository root, build and copy the optimized artifact with:

```sh
RUSTUP_TOOLCHAIN=1.92.0 \
RUSTC_WRAPPER=sccache \
CARGO_TARGET_DIR=target/fixture-contracts \
stellar contract build \
  --manifest-path fixtures/contracts/soroswap-wrapper/Cargo.toml \
  --out-dir fixtures/wasm \
  --optimize \
  --locked
```

Artifact: `fixtures/wasm/kanatoko_soroswap_wrapper.wasm`

SHA-256: `1e7bca07c5aa5c2f31544857027799285dff97266ed7895cf21d0e6736dcb3a8`

The committed artifact is 5,211 bytes and was built from `soroban-sdk 28.0.0`
(`48d506712f964094d14176e2f0b02afcd1054567`) with Stellar CLI 27.0.0
(`5a7c5fe76530bf4248477ac812fc757146b98cc4`) and rustc 1.92.0 for
`wasm32v1-none`, with `RUSTC_WRAPPER=sccache`.
