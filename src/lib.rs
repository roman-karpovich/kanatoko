//! Kanatoko local Soroban fork runtime.
//!
//! The default build exposes deterministic fixture loading, production WASM
//! registration, stateful execution, and checkpoint/revert. The optional
//! `capture` feature adds a one-scenario mainnet runner: generated clients and
//! dynamic calls drive automatic address-first capture, then the same body
//! replays strictly offline. Imported client WASM supplies ABI bindings only;
//! captured network instances and WASM always execute.
//!
//! Kanatoko does not emulate transactions or claim network-faithful execution.
//!
//! Capture contains scenario panics as returned errors, but Rust's standard
//! panic hook still runs and can print panic payloads. Scenario panic messages
//! must therefore never contain credentials or other secrets.

mod fixture;
mod runtime;

#[cfg(feature = "capture")]
mod auto;
#[cfg(feature = "capture")]
mod capture;
#[cfg(feature = "capture")]
mod strict;

#[cfg(feature = "capture")]
pub use auto::{mainnet, AutoRun, AutoRunError, AutoRunner, CacheStatus, ScenarioFork};
#[cfg(feature = "capture")]
pub use capture::{
    CaptureBuilder, CaptureError, CaptureProvenance, CaptureReport, CapturedFixture,
};
#[cfg(feature = "capture")]
pub use strict::{
    AppliedAuthMode, AuthMode, AuthorizationTree, CandidateInstallMode, CandidateRegistration,
    DetachedEvent, ExecutionMode, InvokeErrorKind, InvokeOutcome, InvokeRequest, LedgerValue,
    Receipt, ReceiptDisposition, StateChange, StrictCheckpoint, StrictFork, StrictForkError,
};

pub use fixture::{
    canonical_ledger_digest, FixtureError, FrozenFixture, SUPPORTED_PROTOCOL_VERSION,
};
pub use runtime::{Checkpoint, Fork, RuntimeError};
