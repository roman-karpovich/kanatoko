# Protocol 29 testnet native XLM fixture

`auto-capture.json` is an exact, point-in-time read-only capture of the native
XLM Stellar Asset Contract on public testnet:

- network passphrase: `Test SDF Network ; September 2015`
- contract: `CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC`
- protocol: 29
- ledger: `4983918`
- ledger hash: `cef7754b3c119c835fe69e9071ea0e1f1f4de26b854195e379c1dd5477f573b1`
- captured at: `2026-10-02T12:12:57Z`
- ledger digest: `d28f50e6342075951ba9b082afc5e892eb42fc901ea0b44bfa7dc7cf55443c4a`
- inventory digest: `66c7b6670b560c5a64731db2b0c1d99705c0a46c01313676ad99319073219967`
- canonical bundle digest: `3b72a2481bb1656a126c2a5c7216b8d9c2e3857fb07b6eaef592a71f04ee6145`
- SHA-256: `b7213fd1a3e48ac9013a7531146429d39d63d529984148ff4971e11343e250c5`

The normal test is strictly offline and proves that the committed bundle
replays native XLM transfers with zero RPC reads. Its old candidate-address
absence records are preserved unchanged. After local deployment IDs moved to
a network-specific hash namespace, a separate test proves that deploying into
this historical capture fails closed on the new, uncaptured address.
Fresh capture/replay of local WASM calling captured contracts is covered by
`auto_runner_deploys_local_wasm_that_calls_captured_contract_offline`.

The preceding Protocol 28 capture in `../native-xlm-p28/` is kept unchanged and
is loaded on this line only by the cross-protocol rejection test.

On the Kanatoko 30 line this bundle is historical: `tests/protocol_boundary.rs`
loads it only to prove that a Protocol 29 capture fails closed on the Protocol
30 Host, with and without the newer-protocol opt-in. The commands below apply
to the Kanatoko 29 line.

Refresh is an explicit read-only operation requiring a network still on
Protocol 29. It never builds, signs, or submits a transaction; it also runs the
local stateful and quote-forwarder candidates to capture their current IDs:

```sh
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test testnet_p29 refresh_protocol_29_testnet_fixture \
  -- --exact --ignored --nocapture
```

Verify offline replay with network access trapped:

```sh
HTTP_PROXY=http://127.0.0.1:9 \
HTTPS_PROXY=http://127.0.0.1:9 \
ALL_PROXY=http://127.0.0.1:9 \
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test testnet_p29 frozen_protocol_29_testnet_replays_offline -- --exact
```
