//! Kanatoko local Soroban fork runtime.
//!
//! The default build exposes deterministic fixture loading, production WASM
//! registration, stateful execution, and checkpoint/revert. The optional
//! `capture` feature adds one-scenario mainnet and testnet runners: generated
//! clients and dynamic calls drive automatic execution-based capture, then the
//! same body replays strictly offline. Imported client WASM supplies ABI
//! bindings only; captured network instances and WASM always execute.
//!
//! Contract calls through the scenario facade and [`StrictFork::invoke`] are
//! SDK test-environment executions, not transactions. With the `capture`
//! feature, [`StrictFork`] can also run a host function transaction-faithfully:
//! simulated and applied through the Host's end-to-end entry points with
//! signed authorization, an enforced footprint, and the captured network's cost
//! model and per-transaction limits ([`StrictFork::apply_transaction`]). Fees
//! are reported, never charged; consensus and envelope signatures are not
//! emulated.
//!
//! Loaders and capture accept exactly [`SUPPORTED_PROTOCOL_VERSION`] by
//! default. An explicit newer-protocol opt-in (for example
//! `AutoRunner::allow_newer_protocol` with the `capture` feature, or
//! [`FrozenFixture::from_snapshot_allowing_newer_protocol`]) also accepts a
//! ledger exactly one protocol ahead. Captures keep the real network protocol;
//! only the ledger protocol given to the Host is downgraded.
//!
//! Capture discovery suppresses panic-hook output and returns scenario panics
//! as opaque errors. Strict replay still uses Rust's standard panic hook, so
//! scenario panic messages must never contain credentials or other secrets.

mod fixture;
mod runtime;

#[cfg(feature = "capture")]
mod auto;
#[cfg(feature = "capture")]
mod capture;
#[cfg(feature = "capture")]
mod strict;
#[cfg(feature = "capture")]
mod transaction;

#[cfg(feature = "capture")]
pub use auto::{
    mainnet, testnet, AutoRun, AutoRunError, AutoRunner, CacheStatus, InvocationReport,
    PreviewAuth, ScenarioFork, StrictRun,
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
#[cfg(feature = "capture")]
pub use transaction::{
    conversion_env, from_sc_val, sdk, symbol, to_sc_val, vec_val, AuthorizationFormat,
    ConversionError, Deployment, Ed25519Signer, FailedApply, IntoArgs, IntoScVal,
    InvalidTransaction, NetworkConfig, PreparedTransaction, ResourceExcess, ResourceFee,
    ResourceReport, Sdk, TransactionCall, TransactionError, TransactionLimits, TransactionReceipt,
    TransactionResource, TransactionSimulation,
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
