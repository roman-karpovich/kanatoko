use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use soroban_ledger_snapshot::LedgerSnapshot;
use soroban_sdk::{
    testutils::{EnvTestConfig, Snapshot, SnapshotSourceInput},
    Address, ConstructorArgs, Env,
};
use thiserror::Error;

use crate::{
    canonical_ledger_digest, fixture::executed_protocol_version, FixtureError, FrozenFixture,
};

static NEXT_FORK_ID: AtomicU64 = AtomicU64::new(1);

/// A checkpoint of ledger state and the SDK address/nonce generators.
///
/// Reverting recreates the owned environment. Events, authorization history,
/// budget, and Host PRNG continuity are intentionally not restored.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    fork_id: u64,
    snapshot: Snapshot,
}

/// An isolated, stateful Soroban test environment.
pub struct Fork {
    id: u64,
    env: Env,
    network_protocol_version: u32,
}

impl Fork {
    /// Creates a new isolated environment from a prevalidated fixture.
    ///
    /// # Panics
    ///
    /// Panics if the process exhausts all nonzero `u64` fork IDs.
    #[must_use]
    pub fn from_fixture(fixture: &FrozenFixture) -> Self {
        let id = NEXT_FORK_ID.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, 0, "Kanatoko fork ID space exhausted");
        let snapshot = fixture.ledger_snapshot().clone();
        Self {
            id,
            network_protocol_version: snapshot.protocol_version,
            env: fork_env(ForkEnvSeed::Ledger(snapshot.into())),
        }
    }

    /// Ledger protocol of the network the fixture was recorded on.
    #[must_use]
    pub const fn network_protocol_version(&self) -> u32 {
        self.network_protocol_version
    }

    /// Ledger protocol the Host executes in this fork.
    ///
    /// It differs from [`Self::network_protocol_version`] only for a fixture
    /// accepted through the opt-in newer-protocol mode.
    #[must_use]
    pub const fn executed_protocol_version(&self) -> u32 {
        executed_protocol_version(self.network_protocol_version)
    }

    /// Provides the `Env` required by generated Soroban clients.
    #[must_use]
    pub const fn env(&self) -> &Env {
        &self.env
    }

    /// Hash-validates and registers production WASM with constructor arguments.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::WasmHashMismatch`] before registration when the
    /// provided bytes do not match `expected_sha256`.
    pub fn register_wasm<A: ConstructorArgs>(
        &self,
        wasm: &[u8],
        expected_sha256: [u8; 32],
        constructor_args: A,
    ) -> Result<Address, RuntimeError> {
        let actual_sha256: [u8; 32] = Sha256::digest(wasm).into();
        if actual_sha256 != expected_sha256 {
            return Err(RuntimeError::WasmHashMismatch {
                expected: expected_sha256,
                actual: actual_sha256,
            });
        }

        Ok(self.env.register(wasm, constructor_args))
    }

    /// Captures ledger state plus SDK deterministic generators.
    #[must_use]
    pub fn checkpoint(&self) -> Checkpoint {
        let mut snapshot = self.env.to_snapshot();
        restore_network_protocol(
            &mut snapshot.ledger.protocol_version,
            self.network_protocol_version,
        );
        Checkpoint {
            fork_id: self.id,
            snapshot,
        }
    }

    /// Replaces the owned environment with the checkpoint state.
    ///
    /// Every SDK value and generated client bound to the pre-revert `Env` is
    /// stale and must be reconstructed against [`Self::env`] by the caller.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CheckpointMismatch`] without changing the
    /// environment when the checkpoint belongs to another fork.
    pub fn revert(&mut self, checkpoint: Checkpoint) -> Result<(), RuntimeError> {
        if checkpoint.fork_id != self.id {
            return Err(RuntimeError::CheckpointMismatch);
        }

        self.env = fork_env(ForkEnvSeed::Snapshot(checkpoint.snapshot));
        Ok(())
    }

    /// Computes the canonical digest of the current ledger state.
    ///
    /// The digest covers the network ledger protocol, so a fresh fork's digest
    /// equals [`FrozenFixture::ledger_digest`] also in the newer-protocol mode.
    ///
    /// # Errors
    ///
    /// Returns a fixture integrity or XDR error if the Host exposes invalid
    /// ledger entries.
    pub fn ledger_digest(&self) -> Result<[u8; 32], FixtureError> {
        canonical_ledger_digest(&network_ledger_snapshot(
            &self.env,
            self.network_protocol_version,
        ))
    }
}

/// What a fork `Env` is created from.
pub(crate) enum ForkEnvSeed {
    /// Ledger info, state source, and base snapshot; fresh SDK generators.
    Ledger(SnapshotSourceInput),
    /// A complete SDK snapshot, keeping its address/nonce generators.
    Snapshot(Snapshot),
}

/// Creates every Kanatoko `Env` from captured or checkpointed ledger state.
///
/// This is the single place where the ledger protocol given to the Host is
/// chosen: a ledger accepted by the opt-in newer-protocol mode (exactly one
/// protocol ahead) executes as [`crate::SUPPORTED_PROTOCOL_VERSION`]; every
/// other protocol is handed over unchanged. Callers keep the network protocol
/// in their own snapshots and digests and read state back through
/// [`network_ledger_snapshot`].
pub(crate) fn fork_env(seed: ForkEnvSeed) -> Env {
    let mut env = match seed {
        ForkEnvSeed::Ledger(mut input) => {
            if let Some(ledger_info) = input.ledger_info.as_mut() {
                ledger_info.protocol_version =
                    executed_protocol_version(ledger_info.protocol_version);
            }
            Env::from_ledger_snapshot(input)
        }
        ForkEnvSeed::Snapshot(mut snapshot) => {
            snapshot.ledger.protocol_version =
                executed_protocol_version(snapshot.ledger.protocol_version);
            Env::from_snapshot(snapshot)
        }
    };
    configure_fork_env(&mut env);
    env
}

/// Reads an `Env`'s current ledger snapshot with the network ledger protocol
/// restored, undoing the execution-only downgrade made by [`fork_env`].
pub(crate) fn network_ledger_snapshot(env: &Env, network_protocol_version: u32) -> LedgerSnapshot {
    let mut snapshot = env.to_ledger_snapshot();
    restore_network_protocol(&mut snapshot.protocol_version, network_protocol_version);
    snapshot
}

/// Maps the protocol an `Env` executes back to the network protocol.
///
/// Only the exact downgrade made by [`fork_env`] is undone; any other value is
/// reported unchanged.
pub(crate) fn restore_network_protocol(protocol_version: &mut u32, network_protocol_version: u32) {
    if *protocol_version == executed_protocol_version(network_protocol_version) {
        *protocol_version = network_protocol_version;
    }
}

pub(crate) fn configure_fork_env(env: &mut Env) {
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    // Soroban SDK testutils serializes authorization evidence in observable
    // shadow mode after each top-level invocation. That bookkeeping must not
    // fail an otherwise valid fork call; normal Budget and independent
    // InvocationResourceLimits remain unchanged.
    env.host()
        .set_shadow_budget_limits(u64::MAX, u64::MAX)
        .expect("a fork Env must accept shadow budget configuration");
}

/// Errors produced while configuring the local runtime.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("candidate WASM SHA-256 mismatch")]
    WasmHashMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },

    #[error("checkpoint belongs to another fork")]
    CheckpointMismatch,
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use soroban_env_host::InvocationResourceLimits;
    use soroban_sdk::{
        testutils::{cost_estimate::NetworkInvocationResourceLimits as _, Address as _},
        Address,
    };

    use super::*;

    mod stateful {
        soroban_sdk::contractimport!(
            file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
            sha256 = "0156a9a840e4147732fcf0479846220840ae4b4281c58354aa31cf70daf6b2ea",
        );
    }

    #[test]
    fn fork_configuration_unbounds_only_shadow_budget() {
        let mut env = Env::default();
        env.host().set_shadow_budget_limits(0, 0).unwrap();
        let normal_limits = normal_budget_limits(&env);

        configure_fork_env(&mut env);

        assert_eq!(normal_budget_limits(&env), normal_limits);
        env.mock_all_auths();
        let contract = env.register(stateful::WASM, (0_i64,));
        let user = Address::generate(&env);
        stateful::Client::new(&env, &contract).authorized_increment(&user, &1);
        assert_eq!(env.auths().len(), 1);
    }

    #[test]
    fn fork_configuration_keeps_invocation_resource_limits_enforced() {
        let mut env = Env::default();
        configure_fork_env(&mut env);
        env.mock_all_auths();
        let contract = env.register(stateful::WASM, (0_i64,));
        let user = Address::generate(&env);
        let mut limits = InvocationResourceLimits::mainnet();
        limits.instructions = 0;
        env.cost_estimate().enforce_resource_limits(limits);

        let result = catch_unwind(AssertUnwindSafe(|| {
            stateful::Client::new(&env, &contract).authorized_increment(&user, &1);
        }));

        assert!(result.is_err());
    }

    fn normal_budget_limits(env: &Env) -> (u64, u64) {
        let budget = env.host().budget_cloned();
        (
            budget.get_cpu_insns_consumed().unwrap() + budget.get_cpu_insns_remaining().unwrap(),
            budget.get_mem_bytes_consumed().unwrap() + budget.get_mem_bytes_remaining().unwrap(),
        )
    }
}
