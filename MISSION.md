# Kanatoko Mission

Kanatoko makes real Stellar network state usable as a local, deterministic
Soroban test environment.

It exists so contract developers can register candidate WASM locally, call
real deployed contracts and captured state, explore multi-call behavior, and
inspect the result without paying for a deployment or mutating a public
network.

The name is a quiet joke: *soroban* is the Japanese abacus; *kanatoko* is the
Japanese anvil. Kanatoko aims to give Soroban developers the productive weight
of an Anvil-like workflow while remaining native to Stellar's execution model.

## What We Believe

- **Stellar semantics come first.** Kanatoko borrows developer ergonomics from
  Foundry, not EVM concepts. Soroban has no ambient `sender`, so Kanatoko will
  not invent `prank` or `impersonate` abstractions.
- **Real code and state beat hand-built mocks.** The primary path executes
  production WASM against captured ledger entries. Test doubles remain an
  explicit choice, never a hidden substitution.
- **Reproducibility is a feature.** A frozen fixture must identify its network,
  ledger, protocol, entries, and contract code, and must replay offline without
  silently consulting a newer network state.
- **Protocol mismatches fail closed.** Kanatoko must never make an unsupported
  ledger appear compatible by silently lowering the protocol version.
- **Cheats must be named honestly.** State overrides and mocked authorization
  are useful, but results must distinguish them from cryptographically and
  transaction-faithful execution.
- **Evidence must say what it proves.** A passing contract test is not
  automatically proof of fees, envelope validity, consensus, or deploy
  readiness.

## What We Are Building

The first product is a Rust-first library and capture workflow for:

- loading reproducible Stellar ledger fixtures;
- registering candidate Soroban WASM locally;
- executing stateful cross-contract calls in the real Soroban Host;
- recording authorization requirements, events, errors, and state changes;
- checkpointing and reverting local state;
- turning exploratory network state into frozen, offline CI fixtures.

M3 now supplies the first honest mutable workflow over an M2 capture. A strict
fork preserves confirmed absence versus unknown state, locally injects
hash-checked candidate production WASM with constructor execution, supports
stateful preview/apply calls and checkpoint/revert, and returns detached XDR
receipts containing result/error, auth evidence, events, diagnostics, state
diff, and digests. The runnable Aquarius acceptance proves a candidate contract
can call the captured pool/SAC graph across several calls with zero replay RPC
reads.

Candidate installation remains an explicitly labelled SDK test cheat. It is
not upload/create transaction evidence. Likewise, record and mock-exact auth
are mock-satisfied behavioral evidence; only enforce mode applies supplied Host
authorization entries, and none of these modes invents a sender abstraction.

A compatible local JSON-RPC server may follow, but it is not the foundation.

## Independence

Kanatoko is an independent implementation built on official Stellar protocol,
SDK, Host, XDR, and RPC interfaces. Foundry/Anvil and existing Soroban fork
experiments are acknowledged prior art, not upstreams that control Kanatoko's
roadmap.

Kanatoko is not affiliated with or endorsed by the Stellar Development
Foundation or Foundry.
