use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{Limits, WriteXdr};
use soroban_ledger_snapshot::{Error as SnapshotError, LedgerSnapshot};
use thiserror::Error;

/// The only ledger protocol accepted by the selected Soroban Host.
pub const SUPPORTED_PROTOCOL_VERSION: u32 = soroban_env_host::VERSION.interface.protocol;

const LEDGER_DIGEST_DOMAIN_V1: &[u8] = b"KANATOKO\0LEDGER-SNAPSHOT\0V1\0";

/// A validated, immutable starting point for independent [`crate::Fork`]s.
#[derive(Clone, Debug)]
pub struct FrozenFixture {
    snapshot: LedgerSnapshot,
    ledger_digest: [u8; 32],
}

impl FrozenFixture {
    /// Parses and validates a `LedgerSnapshot` file before an `Env` exists.
    ///
    /// # Errors
    ///
    /// Returns a typed read or parse error, or any validation error documented
    /// by [`Self::from_snapshot`].
    pub fn from_file(
        path: impl AsRef<Path>,
        expected_network_passphrase: &str,
    ) -> Result<Self, FixtureError> {
        let path = path.as_ref();
        let snapshot = LedgerSnapshot::read_file(path).map_err(|source| match source {
            source @ SnapshotError::Io(_) => FixtureError::Read {
                path: path.to_path_buf(),
                source,
            },
            SnapshotError::Serde(error) if error.is_io() => FixtureError::Read {
                path: path.to_path_buf(),
                source: SnapshotError::Serde(error),
            },
            SnapshotError::Serde(error) => FixtureError::Parse {
                path: path.to_path_buf(),
                source: SnapshotError::Serde(error),
            },
        })?;
        Self::from_snapshot(snapshot, expected_network_passphrase)
    }

    /// Validates protocol, network, key integrity, and canonical digest.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported protocol, a network ID mismatch,
    /// malformed key/entry pairs, duplicate keys, or unencodable XDR.
    pub fn from_snapshot(
        snapshot: LedgerSnapshot,
        expected_network_passphrase: &str,
    ) -> Result<Self, FixtureError> {
        if snapshot.protocol_version != SUPPORTED_PROTOCOL_VERSION {
            return Err(FixtureError::UnsupportedProtocol {
                found: snapshot.protocol_version,
                supported: SUPPORTED_PROTOCOL_VERSION,
            });
        }

        let expected_network_id: [u8; 32] =
            Sha256::digest(expected_network_passphrase.as_bytes()).into();
        if snapshot.network_id != expected_network_id {
            return Err(FixtureError::NetworkMismatch {
                expected: expected_network_id,
                found: snapshot.network_id,
            });
        }

        let ledger_digest = canonical_ledger_digest(&snapshot)?;
        Ok(Self {
            snapshot,
            ledger_digest,
        })
    }

    /// Digest of the fixture ledger, excluding SDK/Host runtime state.
    #[must_use]
    pub const fn ledger_digest(&self) -> [u8; 32] {
        self.ledger_digest
    }

    /// Validated source snapshot used to initialize each isolated fork.
    #[must_use]
    pub const fn ledger_snapshot(&self) -> &LedgerSnapshot {
        &self.snapshot
    }
}

/// Computes the version-1 canonical ledger digest.
///
/// The digest is SHA-256 over a domain/version prefix, every `LedgerSnapshot`
/// metadata field, and the entry count followed by entries ordered by
/// canonical `LedgerKey` XDR. Each entry encodes length-prefixed key XDR,
/// length-prefixed `LedgerEntry` XDR, and an explicit optional TTL. It excludes
/// SDK generators, Host PRNG, authorization, events, and budget state.
///
/// # Errors
///
/// Returns an error when a supplied key does not match its entry, a canonical
/// key occurs more than once, or XDR encoding fails.
pub fn canonical_ledger_digest(snapshot: &LedgerSnapshot) -> Result<[u8; 32], FixtureError> {
    let mut encoded_entries = Vec::with_capacity(snapshot.ledger_entries.len());

    for (index, (key, (entry, live_until))) in snapshot.ledger_entries.iter().enumerate() {
        let derived_key = entry.to_key();
        if key.as_ref() != &derived_key {
            return Err(FixtureError::LedgerKeyMismatch { index });
        }

        let key_xdr = key.to_xdr(Limits::none())?;
        let entry_xdr = entry.to_xdr(Limits::none())?;
        encoded_entries.push((key_xdr, entry_xdr, *live_until));
    }

    encoded_entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for pair in encoded_entries.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(FixtureError::DuplicateLedgerKey {
                key_xdr: pair[0].0.clone(),
            });
        }
    }

    let mut digest = Sha256::new();
    digest.update(LEDGER_DIGEST_DOMAIN_V1);
    digest.update(snapshot.protocol_version.to_be_bytes());
    digest.update(snapshot.sequence_number.to_be_bytes());
    digest.update(snapshot.timestamp.to_be_bytes());
    digest.update(snapshot.network_id);
    digest.update(snapshot.base_reserve.to_be_bytes());
    digest.update(snapshot.min_persistent_entry_ttl.to_be_bytes());
    digest.update(snapshot.min_temp_entry_ttl.to_be_bytes());
    digest.update(snapshot.max_entry_ttl.to_be_bytes());
    digest.update((encoded_entries.len() as u64).to_be_bytes());

    for (key_xdr, entry_xdr, live_until) in encoded_entries {
        digest.update((key_xdr.len() as u64).to_be_bytes());
        digest.update(key_xdr);
        digest.update((entry_xdr.len() as u64).to_be_bytes());
        digest.update(entry_xdr);
        match live_until {
            Some(sequence) => {
                digest.update([1]);
                digest.update(sequence.to_be_bytes());
            }
            None => digest.update([0]),
        }
    }

    Ok(digest.finalize().into())
}

/// Fail-closed errors produced before constructing a Soroban environment.
#[derive(Debug, Error)]
pub enum FixtureError {
    #[error("failed to read ledger snapshot {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: SnapshotError,
    },

    #[error("failed to parse ledger snapshot {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: SnapshotError,
    },

    #[error("unsupported ledger protocol {found}; pinned Host supports exactly {supported}")]
    UnsupportedProtocol { found: u32, supported: u32 },

    #[error("fixture network ID does not match the expected passphrase")]
    NetworkMismatch { expected: [u8; 32], found: [u8; 32] },

    #[error("ledger entry at index {index} does not match its supplied key")]
    LedgerKeyMismatch { index: usize },

    #[error("duplicate canonical ledger key")]
    DuplicateLedgerKey { key_xdr: Vec<u8> },

    #[error("failed to encode canonical ledger XDR")]
    Xdr(#[from] soroban_env_host::xdr::Error),
}
