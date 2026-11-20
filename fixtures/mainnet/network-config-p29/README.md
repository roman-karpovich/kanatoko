# Protocol 29 Mainnet network configuration fixture

`auto-capture.json` is an exact, point-in-time read-only capture with
`capture_network_config()`. Its scenario calls `decimals` on the native XLM
Stellar Asset Contract and captures the keys that later transactions read, so
the bundle holds:

- the native XLM contract instance and the 15 Soroban `ConfigSetting` entries
  of `NetworkConfig::REQUIRED_SETTINGS` (17 present entries with one existing
  account);
- the confirmed absence of the test signers' accounts (labels `relayer`,
  `user`, `stranger`, `deployer`, `sender`, `receiver`, `spare`, `poor`), of
  the stateful candidate's `ContractCode` key, and of the two test deployment
  addresses (11 absent entries);
- the account of the well-known seed `[1; 32]`, which exists on Mainnet and
  proves that `create_local_account` never shadows a real account.

Capture details:

- network passphrase: `Public Global Stellar Network ; September 2015`
- protocol: 29
- ledger: `65583876`
- ledger hash: `a306715ad0028d60276a3acaa8ab8c55cfe0a8140d6da86bca0af58faf9a9010`
- ledger close time: `2026-11-20T20:10:24Z`
- ledger digest: `30a43414329f6b1a845d31518103676d528c33467c2dd39cf10f32f424e0efe2`
- inventory digest: `747fbe20b6f408f47be5a0240fc485786651d413db2ae93c744af6245c5eb13c`
- canonical bundle digest: `93cb5139ef8626abedbcde31da2cee720868e4a46176522937f449573b0aa699`
- SHA-256: `a20c44f7737095c8814492138683187d60a4f120624b10c4ae30f4387a5b0450`

`fee-golden.json` holds the envelopes, results, and metadata of four
successful Mainnet Soroban transactions from ledgers 65583873 to 65583876,
read from `getTransactions` of `https://mainnet.sorobanrpc.com`: a small
contract invoke, an invoke without rent, an invoke paying rent, and a fee
bump whose inner transaction uses `AddressV2` credentials. They share the
captured configuration, including the live Soroban state size window, and
the library's unit tests prove that the fee components recomputed from them
equal what the network charged. SHA-256:
`2bef86e7b56e6007c34a0bb493d24ef068272894c401aa168e6a2715f522bf01`.

`tests/strict_transaction.rs` replays the capture offline and runs signed,
footprint-enforced transactions against it. Signing keys, their accounts, and
the candidate contract are explicit local state, not network observations.

Refresh is an explicit read-only operation requiring a network still on
Protocol 29. It never builds, signs, or submits a transaction. A refresh
changes the configuration window, so the golden transactions must be
replaced with transactions from the same 64-ledger window:

```sh
RUSTC_WRAPPER=sccache cargo test --locked --all-features \
  --test strict_transaction refresh_mainnet_network_config_fixture \
  -- --exact --ignored --nocapture
```
