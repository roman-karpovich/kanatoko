# Protocol 30 Mainnet strict-run fixture

The read-only capture at ledger 65648944 (e46c723be37fdbf61742ad1eed67d61753caf6e6b6a2ecfe7bd49c5588171109) used https://mainnet.sorobanrpc.com.
It contains 16 present and 6 absent entries, discovered in 2 rounds. Final replay used 0 RPC reads.

Capture SHA-256: `ffe73cca64d0c13e8f660e82fed98797036cae7d2e8ccf78c23c365916f914f8`.

The capture follows the full strict scenario: deployment, source and address authorization, successful and failed applies, local overrides and a signed SAC transfer to a Protocol 30 muxed contract address. Funding, deployment and signatures are local to the fork.

Refresh with `cargo test --locked --all-features --test strict_run refresh_strict_run_fixture -- --exact --ignored`. Replay with `cargo test --locked --offline --all-features --test strict_run`.
