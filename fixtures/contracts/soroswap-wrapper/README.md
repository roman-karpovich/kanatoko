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

SHA-256: `5540c23481528b3b37004124d3548d669ea1b7ce4121ed7f909c4cb164cf6588`

The committed artifact is 5,113 bytes and was built from `soroban-sdk 30.0.0`
(`414dd7dd95e6798db7d23899f94187017e41077d`) with Stellar CLI 30.0.1
(`f2a7b74175c34585f3b30a1abf42a2d41cbcc390`) and rustc 1.92.0 for
`wasm32v1-none`, with `RUSTC_WRAPPER=sccache`.
