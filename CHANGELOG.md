# Changelog

All notable changes to Kanatoko are documented in this file.

## 29.2.0 - Unreleased

- Add transaction-faithful execution on `StrictFork`. `TransactionCall`
  describes one `InvokeHostFunctionOp` transaction (contract call, WASM
  upload, `CreateContractV2` with constructor arguments, or any
  `HostFunction`) from a source account whose key the test holds, with an
  SDK-default envelope that `with_inclusion_fee`, `with_time_bounds`,
  `with_memo`, `with_muxed_source`, and `with_signature_count` change.
- Simulate and prepare with `soroban-simulation`, the preflight library of
  Stellar RPC `simulateTransaction`, against the fork's last closed ledger
  and with RPC's default resource and fee adjustments
  (`simulate_transaction`, `prepare_transaction`, `sign_transaction`).
- Apply as Stellar Core does at Protocol 29 (`apply_transaction`): refuse
  invalid transactions with `TransactionError::Invalid(InvalidTransaction)`
  and no change (per-transaction limits, footprint key types, duplicates and
  archived-entry indices, memo or muxed source, resource and inclusion fees,
  time bounds, source existence, threshold, and available balance, frozen
  ledger keys); otherwise close the next ledger, charge the source the
  resource fee plus the inclusion fee capped at the base fee, advance its
  sequence number, execute with the enforcing footprint and a module cache,
  fail with Core's result code on archived entries, read, write, event, entry
  size, or refundable-fee limits (`TransactionError::Rejected`), restore
  only marked entries that are still archived, and refund the unused
  refundable fee. Receipts carry the result code, the exceeded
  resource, the fee charged, the ledger, and measured resources and fees.
- Sign authorization with `Ed25519Signer` (`from_seed`, `from_label`):
  `sign_authorization` and `authorize` produce the SDKs'
  `SorobanCredentials::Address`, and `sign_authorization_as` and
  `authorize_as` also produce `AddressV2` (`AuthorizationFormat`).
  `AuthorizationTree::source_account_authorization` builds source-account
  credentials, and `StrictFork::invoke_with_source` names the transaction
  source for `invoke` without changing `invoke`.
- Add negative-path helpers on `PreparedTransaction`
  (`with_tampered_signature`, `with_authorization`,
  `without_authorization_entry`, `without_footprint_key`,
  `with_transaction_data`, `with_call`) and `StrictFork::expect_failed_apply`,
  which requires a network failure and proves it changed nothing, or only the
  source's fee and sequence number.
- Add explicit local setup and reads on `StrictFork`: `create_local_account`,
  which requires the account key to be captured absent, `set_contract_data`,
  `ledger_entry`, `contract_address`, `network_id`, `ledger_sequence`, and
  `ledger_close_time`.
- Add `CaptureBuilder::capture_network_config` and
  `AutoRunner::capture_network_config`, which capture the 15 Soroban
  `ConfigSetting` entries in `NetworkConfig::REQUIRED_SETTINGS` with the
  scenario's ledger, and `ScenarioFork::capture_signer`, `capture_wasm`,
  `capture_deployed_contract`, and `capture_ledger_key` for keys only later
  transactions read. An online runner recaptures a cache without network
  settings. Such bundles also carry the `StateArchival` entry, checked
  against the ledger TTL bounds; releases before 29.2.0 reject them.
- Add `AutoRunner::run_strict`, which runs a scenario on a discovering
  `StrictFork`: every uncaptured key is recorded and read from the network
  at the round's anchor ledger, or treated as absent if the anchor moved, so
  one pass follows dependency chains. The recorded keys are rematerialized
  at a new coherent ledger and the pass reruns until it records none, then
  the frozen bundle replays strictly; offline it only replays. Bundles may
  record up to 64 rounds. Network settings, signer
  account absence, uploaded code, deployment addresses, and failure-path
  keys are discovered without `capture_*` calls; keys missing from a
  declared footprint still fail in the Host. The bundle reaches the cache
  only after a final pass that does not panic. `StrictRun` holds the value
  and the fixture.
- Add a typed layer: `IntoScVal` and `IntoArgs` for `Env`-free arguments,
  `symbol`, `vec_val`, `TransactionCall::invoke_fn`, and on `StrictFork`
  `sign`, `apply_signed`, `deploy` (`Deployment`), `create_local_accounts`,
  `view`, `token_balance`, `account_balance`, `set_token_balance`,
  `contract_data`, `update_contract_data`, `contract_instance`, and
  `scoped`. `sign` derives nonces from the network, the signing address, and
  a counter; `set_token_balance` mirrors the contract's and Core's flags,
  reserves, and liabilities and refuses unreachable balances; `PreparedTransaction::resigned_by`, `with_function`, and
  `with_args`; `TransactionReceipt::events_from` and `Display`;
  `DetachedEvent::topics`, `data`, `is_from`, `topic_as`, and `data_as`;
  `TransactionReceipt::single_event`; `ResourceReport::check_within` and
  `TransactionLimits::scaled`; `StrictFork::expect_rejected_with`;
  `FailedApply::host_error`. `conversion_env`, `to_sc_val`, `from_sc_val`,
  and `sdk` convert SDK values such as contract types to and from XDR in a
  per-thread environment, which `view`, `contract_data`, and
  `update_contract_data` also decode in.
  `set_contract_data` takes any `IntoScVal` key.
- Add the Protocol 29 Mainnet `strict-run-p29` capture of one `run_strict`
  flow, replayed in parts offline.
- Add the Protocol 29 Mainnet `network-config-p29` capture and the fees of
  four real Mainnet Soroban transactions, which the recomputed fees match.
- New errors use the non-exhaustive `TransactionError` and
  `InvalidTransaction`. Existing public types and their variants are
  unchanged. Depend on `soroban-simulation` 29 and `ed25519-dalek` under the
  `capture` feature.

## 29.1.2 - 2026-11-16

- Derive local `ScenarioFork::deploy` IDs from a network-specific hash
  namespace and the deterministic SDK address sequence. Previously, a common
  SDK counter address without a contract instance could already hold XLM or
  other token balances, which a newly registered candidate would inherit.
- Local candidate IDs change. A previously captured scenario using `deploy`
  may need a fresh capture on a compatible live protocol before replaying on
  this version. Unknown keys still fail closed; historical captures are not
  rewritten or treated as proof of absence for the new IDs. Keep the original
  Kanatoko version pinned when preserving an old candidate-address replay.

## 29.1.1 - 2026-11-10

- Report ledger XDR that this release cannot decode, read from a network on
  another protocol, as `UnsupportedProtocol` for that protocol instead of
  `CaptureError::Transport`. Protocol 30 changed the ledger header format, so
  a capture with `allow_newer_protocol()` from a Protocol 30 network failed
  with `Transport { operation: "getLatestLedger" }`; it now fails with
  `UnsupportedProtocol { found: 30, supported: 29 }`. At Protocol 29 an
  undecodable header or entry is still a `Transport` error.
- The `UnsupportedProtocol` message of `CaptureError` and `FixtureError` now
  names the Kanatoko release line to use. No API changes.
- Document that the 29 line cannot capture from Protocol 30 networks
  (testnet now, Mainnet after its upgrade); use `kanatoko = "=30.0.0-rc.2"`
  for them until 30.0.0 is released. Committed bundles still replay offline.
- Lock the tests to `soroban-sdk`, `soroban-env-host` and
  `soroban-ledger-snapshot` 29.0.1, the versions a fresh resolution picks;
  `soroban-env-host` 29.0.0 is yanked.
- Add `examples/offline_replay.rs`, which replays the committed Soroswap
  XLM/USDC Mainnet capture without RPC.
- Run the CI checks on pushes to `master` and `sdk-*` and on pull requests.
- Document controlling ledger time in scenarios, replace the README roadmap,
  and remove the README rows for Kanatoko 25 and 26, which were never
  published.

## 29.1.0 - 2026-11-09

- Replace the Mainnet pool fixture and CLI scenario with a Soroswap XLM/USDC
  scenario captured on Protocol 29, and remove the scenario-specific capture
  binary. Rename the CLI scenario to `soroswap-xlm-usdc`.

## 29.0.0 - 2026-10-07

- Add Protocol 29 support on the official `soroban-sdk` and
  `soroban-ledger-snapshot` 29.0.0 with the compatible Host 29.0.0 runtime.
  The 29 line is published on crates.io as `kanatoko = "29"`.
- Drop the temporary `soroban-sdk` fork used by `29.0.0-alpha.1`. Every
  dependency comes from crates.io again, with the same broad major ranges as
  the other stable lines. No Kanatoko API changes; SDK 29 has no migration
  steps from SDK 28.
- Carry over the opt-in newer-protocol mode from 28.1.0. It is defined
  relative to the Host protocol, so on the 29 Host it accepts exactly
  Protocol 30 and executes it as Protocol 29. Protocol 31 and Protocols 28
  and older still fail closed with `UnsupportedProtocol`, also with the
  opt-in. No Protocol 30 network exists yet, so acceptance is covered by
  synthetic in-memory Protocol 30 ledgers and captures.
- Keep the fixtures of the alpha: the Protocol 29 Mainnet `capture.json` and
  `auto-capture.json` and the `native-xlm-p29` testnet capture are primary.
  `capture-p27.json`, `capture-p28.json`, and the Protocol 28 testnet capture
  remain cross-protocol rejection evidence. The 28.1.0 copies
  `capture-p29.json` and `auto-capture-p29.json` are not carried over: they
  are byte-identical to the primaries on this line.
- Keep the committed candidate fixtures built with `soroban-sdk` 28; the
  Protocol 29 Host executes them unchanged.

## 29.0.0-alpha.1 - 2026-10-02

- Add alpha Protocol 29 support on the published `soroban-env-host` 29.0.0
  Host. This release is a GitHub prerelease only and is not published on
  crates.io; depend on it by git tag `v29.0.0-alpha.1`.
- No official `soroban-sdk` 29 exists yet, so take `soroban-sdk` and
  `soroban-ledger-snapshot` from a temporary fork of `soroban-sdk` 28.0.0
  (`roman-karpovich/rs-soroban-sdk`, tag `v29.0.0-kanatoko.1`) that only moves
  the env crates to 29.0.0. The 29 line returns to crates.io dependencies once
  the official SDK 29 ships.
- Refresh the execution-driven Mainnet pool captures on Protocol 29 and
  retain the prior Protocol 28 bundle as `capture-p28.json`, next to
  `capture-p27.json`, as explicit cross-protocol rejection evidence.
- Add frozen Protocol 29 testnet evidence for the native XLM Stellar Asset
  Contract and keep the Protocol 28 testnet capture as rejection evidence.
- Keep the committed candidate fixtures built with `soroban-sdk` 28; the
  Protocol 29 Host executes them unchanged.
- Teach release automation to verify `-alpha.N` tags, accept the `sdk-29`
  release line, and publish alphas only as GitHub prereleases, never to
  crates.io.

## 28.1.0 - 2026-10-07

- Add an opt-in newer-protocol mode that captures and replays a ledger exactly
  one protocol ahead of the Host (Protocol 29 on the 28 line) while no
  matching Kanatoko line exists: `AutoRunner::allow_newer_protocol`,
  `CaptureBuilder::allow_newer_protocol`,
  `CapturedFixture::from_file_allowing_newer_protocol`,
  `FrozenFixture::from_file_allowing_newer_protocol`,
  `FrozenFixture::from_snapshot_allowing_newer_protocol`, and
  `--allow-newer-protocol` on the CLI `capture` and `run` commands.
- Keep captures truthful: bundles, provenance, the bundled ledger snapshot, and
  every digest record the real network protocol. Only the ledger protocol
  handed to the Host is downgraded, in one place for every `Env`.
- Expose the network and executed protocols through
  `network_protocol_version` and `executed_protocol_version` on `Fork`,
  `StrictFork`, `ScenarioFork`, and `InvocationReport`, plus
  `CaptureProvenance::executed_protocol_version` and
  `FrozenFixture::executed_protocol_version`. The CLI prints a one-line warning
  and adds both protocols to its report and receipts in this mode.
- Keep rejecting older protocols and protocols two or more ahead with the
  existing `UnsupportedProtocol` errors. Without the opt-in, behaviour, errors,
  output, and the bundle format are unchanged.
- Add the Protocol 29 Mainnet and testnet captures from the 29 line as
  `capture-p29.json`, `auto-capture-p29.json`, and `native-xlm-p29`, with
  offline acceptance on the Protocol 28 Host. The strict Mainnet pool workflow
  returns the same quotes as on a Protocol 29 Host.

## 28.0.0 - 2026-09-21

- Move the Protocol 28 line to stable `soroban-sdk` and
  `soroban-ledger-snapshot` 28.0.0 with the compatible Host 28.0.2 runtime.
- Rebuild the committed Protocol 28 candidate fixtures with stable SDK
  metadata and update every pinned artifact hash.
- Refresh the execution-driven Mainnet pool captures on Protocol 28,
  restore their offline acceptance coverage, and retain the prior Protocol 27
  bundle as explicit cross-protocol rejection evidence.
- Keep the frozen Protocol 28 testnet fixture unchanged and verify its local
  stable-SDK candidates against the same captured ledger.

## 28.0.0-rc.1 - 2026-08-30

- Add prerelease Protocol 28 support with exact `soroban-sdk` and
  `soroban-ledger-snapshot` 28.0.0-rc.1 plus `soroban-env-host` 28.0.2 pins.
- Capture and validate the complete CAP-85 external-executable closure from
  contract instance through the owner's executable tag to referenced WASM.
- Fail closed on absent or malformed executable references and on absent
  referenced ContractCode, including after a changed-ledger refresh.
- Allow `replace_wasm` to detach an external reference locally while preserving
  instance storage, TTL, and the untouched reference entry.
- Add exact frozen Protocol 28 testnet evidence for the native XLM Stellar
  Asset Contract and rebuild local candidate fixtures against SDK 28 RC.
- Keep Protocol 27 mainnet captures unchanged and retain their explicit
  cross-protocol rejection test while Mainnet remains on Protocol 27.
- Teach release automation to verify and publish an `-rc.N` tag as a GitHub
  prerelease on the `sdk-28` release line.

## 27.0.2 - 2026-07-23

- Bind cached state values to the complete captured ledger anchor and probe the
  current anchor before every online cache hit.
- Reuse only the cached ledger-key inventory across ledgers, refreshing all
  known values in coherent batches before fixed-point dependency discovery.
- Keep `.offline()` as a zero-transport pinned replay and `.refresh()` as an
  explicit cold capture that discards the cached inventory.

## 27.0.1 - 2026-07-23

- Retry throttled or transient read-only RPC responses with a short bounded
  backoff while honoring reasonable numeric `Retry-After` values.
- Stop issuing RPC requests after the first exhausted capture transport
  failure.
- Suppress the misleading inner Host panic when a captured read ends in a
  typed transport failure.
- Add an opt-in read-RPC rate limit; capture remains unlimited by default.

## 27.0.0 - 2026-07-23

- Align the Kanatoko release major with its Soroban SDK, Host, ledger snapshot,
  and supported ledger protocol.
- Keep Soroban dependencies on broad same-major ranges so downstream test
  harnesses resolve one compatible runtime.
- Prepare release automation and documentation for maintained SDK lines 25,
  26, and 27.
- Add `ScenarioFork::replace_wasm` for testing candidate code at an existing
  captured address without changing its storage, TTL, or running a constructor.
- Prevent SDK authorization-evidence bookkeeping from exhausting the Host
  shadow budget without weakening contract execution or invocation limits.

## 0.1.0 - 2026-07-23

- Capture coherent mainnet or testnet state from the contracts, accounts,
  trustlines, WASM, and storage touched by a Rust scenario.
- Replay captured state locally in one mutable Soroban environment with no RPC
  fallback.
- Mix captured network contracts, dynamic invocations, generated clients, and
  locally deployed candidate WASM.
- Preview mutating calls with detached results, authorization, events, state
  changes, and resource estimates.
- Fail closed on unsupported protocol versions, unknown ledger keys, and
  incomplete offline fixtures.
