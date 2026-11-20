# Protocol 29 Mainnet strict-run fixture

`auto-capture.json` is the frozen fixture of the `full` flow in
`tests/strict_run.rs`, captured read-only with `AutoRunner::run_strict` in 3
discovery rounds. It holds the 15 Soroban `ConfigSetting` entries and the
native XLM Stellar Asset Contract instance (16 present entries), and the
confirmed absence of the two test signers' accounts, the stateful WASM's
code key, the deployment address, and the XLM balance key of that address
(6 absent entries). Signers, their accounts, the deployed contract, and every
balance are explicit local state, not network observations.

- network passphrase: `Public Global Stellar Network ; September 2015`
- protocol: 29
- ledger: `65585041`
- ledger hash: `cd2eab05b953b4c1e622a56c6af87a8e55e8d133b7883ee6acff0144a1677fba`
- ledger digest: `fe22b7623d6f40f1fb60d227acdb8c77950356439a533b15672ef1995fa3fd10`
- inventory digest: `025f8ef07b29db76f948f94fb2a0a7c1e9a17f0f2aca10d64967ab781238b98c`
- canonical bundle digest: `61fc510d24dcc6de141dfb7359a9e4cafe9ee7018bd6243c88fec121c338802e`
- SHA-256: `ab99de42030a4e46f1f5615c455ffada7d3c8571118d22d018a8b826f11baea0`

The other tests replay parts of the flow offline against it. Refresh with:

```sh
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test strict_run refresh_strict_run_fixture -- --exact --ignored --nocapture
```
