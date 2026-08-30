# Protocol 28 testnet native XLM fixture

`auto-capture.json` is an exact, point-in-time read-only capture of the native
XLM Stellar Asset Contract on public testnet:

- network passphrase: `Test SDF Network ; September 2015`
- contract: `CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC`
- protocol: 28
- ledger: `4414462`
- ledger hash: `e8956fc2ac30d2cad7be81bef50c52d7402dbd6564aeef9dfcefc7e909f5dcc1`
- captured at: `2026-08-30T13:18:17Z`
- ledger digest: `d50c3438ba1a6db7fc57fc79b40343a6cba17aefdce6c04d735cb8897488ac97`
- inventory digest: `3c365ed2d975598a08875290d5cb3e7e5551db13985eb2e9f76f6175adf3aff8`
- canonical bundle digest: `e69ceb076f9ed0babd88c61a15bb554a7717fb670e577454ef8fa596f0aaca4d`

The normal test is strictly offline and proves that the committed bundle
replays with zero RPC reads. It also deploys and executes the committed
Protocol 28 stateful and Aquarius-wrapper fixtures only inside Kanatoko's
local Host; no contract is uploaded or deployed to testnet.

Refresh is an explicit read-only operation. It never builds, signs, or submits
a transaction:

```sh
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test testnet_p28 refresh_protocol_28_testnet_fixture \
  -- --exact --ignored --nocapture
```

Verify offline replay with network access trapped:

```sh
HTTP_PROXY=http://127.0.0.1:9 \
HTTPS_PROXY=http://127.0.0.1:9 \
ALL_PROXY=http://127.0.0.1:9 \
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test testnet_p28 frozen_protocol_28_testnet_replays_offline -- --exact
```
