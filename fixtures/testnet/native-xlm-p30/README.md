# Protocol 30 testnet native XLM fixture

`auto-capture.json` is an exact, point-in-time read-only capture of the native
XLM Stellar Asset Contract on public testnet:

- network passphrase: `Test SDF Network ; September 2015`
- contract: `CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC`
- protocol: 30
- ledger: `5898217`
- ledger hash: `ed13d42c509e427da7954bb23a1f316d95b00d0a2c62b061c301e3552a3188c1`
- captured at: `2026-11-24T10:05:27Z`
- discovery: 2 rounds, 1 present, 4 confirmed absent
- ledger digest: `4f4868dd74bd4ef4a337b7ab99fa202a1dcaa809ec91f44503f9bd3ef6720d38`
- inventory digest: `38845692ef455275b7e97469fe7615a21acfc7bf6bf57918646b2ba57ec89e97`
- canonical bundle digest: `9da88a052a529315aff102242ea3e2d45ef9a06da47ce8a54488fdc9a1e7fcf9`
- SHA-256: `1cfc8f4f33f5aad0110395b03ebe52b3063b3ca543798d8ff35a02277f712872`

The one present entry is the native XLM contract instance. The four
confirmed-absent entries are the two local accounts of the XLM transfer,
`GAMBIXNU5PSAVZNPCAMV3GRTHUEDZLDNL37IJ3U5EWM6E2Z76NT6CUZW` and
`GARDMYFXZJWNWVA4WSQQ6EGBK56GOB42SXYEEL2YLUXR2GB6LZRC3LAP`, and the contract
instances at the two local candidate IDs,
`CANLHRJ2NQWHJNANC7WAMOCLOBM7SQKMKBBLGNJXDZIJSQY3YTTICCS2` and
`CDQLCJN4M6ZL3YWO65JVG3HG3FM3CGUGSEUJKEC65XCEAYI7CNUTQQQM`. The candidate IDs
come from Kanatoko's network-scoped local deployment namespace, and the
capture proves that no contract exists at them on testnet.

The normal test is strictly offline and proves that the committed bundle
replays with zero RPC reads. It also deploys and executes the committed
stateful and quote-forwarder fixtures (built with `soroban-sdk` 30) only
inside Kanatoko's local Protocol 30 Host; no contract is uploaded or deployed
to testnet.

The preceding Protocol 29 capture in `../native-xlm-p29/` and the Protocol 28
capture in `../native-xlm-p28/` are kept unchanged and are loaded on this line
only by the cross-protocol rejection tests.

Refresh is an explicit read-only operation requiring a network still on
Protocol 30. It never builds, signs, or submits a transaction:

```sh
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test testnet_p30 refresh_protocol_30_testnet_fixture \
  -- --exact --ignored --nocapture
```

Verify offline replay with network access trapped:

```sh
HTTP_PROXY=http://127.0.0.1:9 \
HTTPS_PROXY=http://127.0.0.1:9 \
ALL_PROXY=http://127.0.0.1:9 \
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test testnet_p30 frozen_protocol_30_testnet_replays_offline -- --exact
```
