# Protocol 29 Mainnet strict-run fixture

`auto-capture.json` is the frozen fixture of the `full` flow in
`tests/strict_run.rs`, captured read-only with `AutoRunner::run_strict` in 7
discovery rounds. It holds the 15 Soroban `ConfigSetting` entries and the
native XLM Stellar Asset Contract instance (16 present entries), and the
confirmed absence of the two test signers' accounts, the stateful WASM's
code key, the deployment address, and the XLM balance key of that address
(6 absent entries). Signers, their accounts, the deployed contract, and every
balance are explicit local state, not network observations.

- network passphrase: `Public Global Stellar Network ; September 2015`
- protocol: 29
- ledger: `65584688`
- ledger hash: `57d0c00c50aeb16b18155816cafb70a289e62763c8b106e80fd87b133bf5a025`
- ledger digest: `356ea0b0588892ae7527e99d72dc7b2c3f4b407712496da6d3903ace485be52e`
- inventory digest: `f67240a89219472c5004ed09747cd073f0506884afdcfc3d6e3529af05b72027`
- canonical bundle digest: `5b91de6be228a20f0918285869201ca32879414e030e911ee8474936080f495d`
- SHA-256: `99b218c6afecac32fc2840fc0d6f621455678e9c217f311a7a44c66c65828d6c`

The other tests replay parts of the flow offline against it. Refresh with:

```sh
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test strict_run refresh_strict_run_fixture -- --exact --ignored --nocapture
```
