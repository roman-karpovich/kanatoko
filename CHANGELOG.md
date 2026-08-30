# Changelog

All notable changes to Kanatoko are documented in this file.

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
