//! Kanatoko frozen Soroban runtime.
//!
//! M0 deliberately exposes only deterministic fixture loading, production
//! WASM registration, stateful execution, and checkpoint/revert. It does not
//! emulate transactions or claim network-faithful execution.

mod fixture;
mod runtime;

pub use fixture::{
    canonical_ledger_digest, FixtureError, FrozenFixture, SUPPORTED_PROTOCOL_VERSION,
};
pub use runtime::{Checkpoint, Fork, RuntimeError};
