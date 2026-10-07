# Protocol 29 testnet native XLM fixture

`auto-capture.json` is the Protocol 29 testnet capture recorded by Kanatoko 29
on a Protocol 29 Host, copied byte for byte. It is an exact, point-in-time
read-only capture of the native XLM Stellar Asset Contract on public testnet:

- network passphrase: `Test SDF Network ; September 2015`
- contract: `CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC`
- protocol: 29
- ledger: `4983918`
- ledger hash: `cef7754b3c119c835fe69e9071ea0e1f1f4de26b854195e379c1dd5477f573b1`
- ledger digest: `d28f50e6342075951ba9b082afc5e892eb42fc901ea0b44bfa7dc7cf55443c4a`
- inventory digest: `66c7b6670b560c5a64731db2b0c1d99705c0a46c01313676ad99319073219967`
- canonical bundle digest: `3b72a2481bb1656a126c2a5c7216b8d9c2e3857fb07b6eaef592a71f04ee6145`
- SHA-256: `b7213fd1a3e48ac9013a7531146429d39d63d529984148ff4971e11343e250c5`

On this line it loads only through the explicit newer-protocol opt-in and
executes on the Protocol 28 Host. Without the opt-in it fails closed with
`UnsupportedProtocol { found: 29, supported: 28 }`. The offline test deploys
and executes the committed stateful and Aquarius-wrapper fixtures only inside
Kanatoko's local Host; no contract is uploaded or deployed to testnet.

Verify offline replay with network access trapped:

```sh
HTTP_PROXY=http://127.0.0.1:9 \
HTTPS_PROXY=http://127.0.0.1:9 \
ALL_PROXY=http://127.0.0.1:9 \
cargo test --locked --all-features --test testnet_p28 \
  frozen_protocol_29_testnet_replays_offline_with_the_newer_protocol_opt_in -- --exact
```
