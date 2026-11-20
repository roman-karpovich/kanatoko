# Protocol 29 Mainnet network configuration fixture

`auto-capture.json` is an exact, point-in-time read-only capture with
`capture_network_config()`. Its scenario only calls `decimals` on the native
XLM Stellar Asset Contract, so the bundle holds that contract's instance and
the 13 Soroban `ConfigSetting` entries of `NetworkConfig::REQUIRED_SETTINGS`:

- network passphrase: `Public Global Stellar Network ; September 2015`
- contract: `CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA`
- protocol: 29
- ledger: `65583322`
- ledger hash: `e6b1a894ff7b976069f68f526b69e281126d5d4d3ce29bc620abebc05a36a32f`
- ledger close time: `2026-11-20T19:24:14Z`
- ledger digest: `545b9de7cfb0d098fadb17afc7aeca12ad5ae055800c953eba68052e009948ec`
- inventory digest: `c387daf6120aa75f0280aad8f4b54daa388173aa17b48fe75dfa6320d9f90a04`
- canonical bundle digest: `78a0ba3a825a32244f1ffc112437c4fda0c50ba2d7bc82483ce932a751b11b19`
- SHA-256: `5309dafae571a9a64619f1f972a962534b87bd10ece7e99b2a857f22494aaa48`

`tests/strict_transaction.rs` replays it offline and runs signed,
footprint-enforced transactions against it: native XLM transfers on the
captured asset contract, the local stateful candidate, and WASM upload and
`CreateContractV2`. Signing keys, their accounts, and the candidate are
explicit local state, not network observations.

Refresh is an explicit read-only operation requiring a network still on
Protocol 29. It never builds, signs, or submits a transaction:

```sh
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test strict_transaction refresh_mainnet_network_config_fixture \
  -- --exact --ignored --nocapture
```
