//! Kanatoko frozen Soroban runtime.
//!
//! The default build exposes deterministic fixture loading, production WASM
//! registration, stateful execution, and checkpoint/revert. The optional
//! `capture` feature adds address-first RPC capture and strict offline replay;
//! it does not emulate transactions or claim network-faithful execution.
//!
//! Capture contains scenario panics as returned errors, but Rust's standard
//! panic hook still runs and can print panic payloads. Scenario panic messages
//! must therefore never contain credentials or other secrets.

mod fixture;
mod runtime;

#[cfg(feature = "capture")]
mod capture;

#[cfg(feature = "capture")]
pub use capture::{
    CaptureBuilder, CaptureError, CaptureProvenance, CaptureReport, CapturedFixture,
};

pub use fixture::{
    canonical_ledger_digest, FixtureError, FrozenFixture, SUPPORTED_PROTOCOL_VERSION,
};
pub use runtime::{Checkpoint, Fork, RuntimeError};
