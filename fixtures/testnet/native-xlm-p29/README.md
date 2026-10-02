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

The normal test is strictly offline and proves that the committed bundle
replays with zero RPC reads. It also deploys and executes the committed
stateful and Aquarius-wrapper fixtures (built with `soroban-sdk` 28) only
inside Kanatoko's local Protocol 29 Host; no contract is uploaded or deployed
to testnet.

The preceding Protocol 28 capture in `../native-xlm-p28/` is kept unchanged and
is loaded on this line only by the cross-protocol rejection test.

Refresh is an explicit read-only operation. It never builds, signs, or submits
a transaction:

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
