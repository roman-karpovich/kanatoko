//! Kanatoko local Soroban fork runtime.
//!
//! Load a saved fixture, run production contract WASM, and checkpoint or revert
//! the local ledger. By default, `multihost::replay` selects Host 29 or 30 from
//! the capture. Select `host-30` with default features disabled to build only
//! the current Host.
//!
//! The `capture` feature provides mainnet and testnet runners that record ledger
//! reads during execution and replay the same scenario offline. Imported client
//! WASM supplies ABI bindings. Captured contract instances and WASM execute.
//!
//! Kanatoko does not emulate transactions or claim network-faithful execution.
//!
//! Single-Host loaders and capture accept exactly [`SUPPORTED_PROTOCOL_VERSION`] by
//! default. An explicit newer-protocol opt-in (for example
//! `AutoRunner::allow_newer_protocol` with the `capture` feature, or
//! [`FrozenFixture::from_snapshot_allowing_newer_protocol`]) also accepts a
//! ledger exactly one protocol ahead. Captures keep the real network protocol;
//! only the ledger protocol given to the Host is downgraded.
//!
//! Capture discovery suppresses panic-hook output and returns scenario panics
//! as opaque errors. Strict replay still uses Rust's standard panic hook, so
//! scenario panic messages must never contain credentials or other secrets.

#[cfg(feature = "host-30")]
pub mod multihost;

mod fixture;
mod runtime;

#[cfg(feature = "capture")]
mod auto;
#[cfg(feature = "capture")]
mod capture;
#[cfg(feature = "capture")]
mod strict;

#[cfg(feature = "capture")]
pub use auto::{
    mainnet, testnet, AutoRun, AutoRunError, AutoRunner, CacheStatus, InvocationReport,
    PreviewAuth, ScenarioFork,
};
#[cfg(feature = "capture")]
pub use capture::{
    CaptureBuilder, CaptureError, CaptureProvenance, CaptureReport, CapturedFixture,
};
#[cfg(feature = "capture")]
pub use strict::{
    AppliedAuthMode, AuthMode, AuthorizationTree, CandidateInstallMode, CandidateRegistration,
    DetachedEvent, ExecutionMode, InvocationFailure, InvokeErrorKind, InvokeOutcome, InvokeRequest,
    LedgerValue, Receipt, ReceiptDisposition, StateChange, StrictCheckpoint, StrictFork,
    StrictForkError,
};

pub use fixture::{
    canonical_ledger_digest, FixtureError, FrozenFixture, SUPPORTED_PROTOCOL_VERSION,
};
pub use runtime::{Checkpoint, Fork, RuntimeError};

// Re-export the selected protocol line so downstream harnesses can avoid
// introducing a second, type-incompatible Env/Host dependency.
pub use soroban_env_host;
pub use soroban_ledger_snapshot;
pub use soroban_sdk;
