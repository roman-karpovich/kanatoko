# Protocol 29 Mainnet strict-run fixture

`auto-capture.json` is the frozen fixture of the `full` flow in
`tests/strict_run.rs`, captured read-only with `AutoRunner::run_strict` in 2
discovery rounds. It holds the 15 Soroban `ConfigSetting` entries and the
native XLM Stellar Asset Contract instance (16 present entries), and the
confirmed absence of the two test signers' accounts, the stateful WASM's
code key, the deployment address, and the XLM balance key of that address
(6 absent entries). Signers, their accounts, the deployed contract, and every
balance are explicit local state, not network observations.

- network passphrase: `Public Global Stellar Network ; September 2015`
- protocol: 29
- ledger: `65585238`
- ledger hash: `b9a9f2027dcfcd3497957701c255de9efea8298391908a60ad59a3f0a6a964db`
- ledger digest: `854dc240cc6f4f6aa0ca2b17b73601b2b587994181478aaa9c4d1c8958d77d03`
- inventory digest: `944b752f8d574c0726192f0afa0638a02b36fa7eb33091ffceb45b9c260b0141`
- canonical bundle digest: `01dccce3d6943c8dc663986a97c76ac0069647921434cdd5865478eac35500fe`
- SHA-256: `af685802ea783f507f325c68ead75452306beb01fdddefa40abe607d81eb4e51`

The other tests replay parts of the flow offline against it. Refresh with:

```sh
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test strict_run refresh_strict_run_fixture -- --exact --ignored --nocapture
```
