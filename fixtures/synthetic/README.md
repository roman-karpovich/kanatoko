# Synthetic older-protocol bundles

These two small files are capture bundles that were never captured from a
network. Each contains an empty ledger snapshot, zero Present and Absent
entries, the Mainnet passphrase, and digests that are valid for the empty
content, written with Kanatoko's internal bundle digest helpers:

- `mainnet-protocol-27.capture.json`: schema-v1 bundle at Protocol 27.
- `mainnet-protocol-28.capture.json`: schema-v2 bundle at Protocol 28.

`provenance.rpc_origin` is `https://synthetic.invalid` and
`provenance.ledger_hash` is `synthetic-no-network-ledger` so the files cannot be
mistaken for network evidence.

They exist only so that older-protocol rejection stays covered without
committing state of a specific pool. `tests/protocol_boundary.rs` and
`tests/cli.rs` assert that loaders, the offline runners, and the CLI reject them
with `UnsupportedProtocol` before any ledger state is used, with and without
the newer-protocol opt-in. Tests that need real Protocol 28 evidence use
`fixtures/testnet/native-xlm-p28/auto-capture.json`.
