# Changelog

All notable changes to Kanatoko are documented in this file.

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
