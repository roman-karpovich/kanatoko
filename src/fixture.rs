use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{Limits, WriteXdr};
use soroban_ledger_snapshot::{Error as SnapshotError, LedgerSnapshot};
use thiserror::Error;

/// The only ledger protocol accepted by the selected Soroban Host.
///
/// Fixture loading and capture accept exactly this protocol by default. The
/// explicit newer-protocol mode (for example
/// [`FrozenFixture::from_snapshot_allowing_newer_protocol`]) additionally
/// accepts exactly one protocol above it, which the Host then executes as this
/// protocol.
pub const SUPPORTED_PROTOCOL_VERSION: u32 = soroban_env_host::VERSION.interface.protocol;

/// The one ledger protocol above the Host's own that the opt-in newer-protocol
/// mode accepts.
const NEWER_PROTOCOL_VERSION: u32 = SUPPORTED_PROTOCOL_VERSION + 1;

/// Which ledger protocols a loader or capture accepts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ProtocolPolicy {
    /// Only [`SUPPORTED_PROTOCOL_VERSION`]; the default without the opt-in.
    #[default]
    Exact,
    /// [`SUPPORTED_PROTOCOL_VERSION`] or exactly one protocol above it.
    AllowNewer,
}

impl ProtocolPolicy {
    /// Fails closed with the existing `UnsupportedProtocol` shape when `found`
    /// is outside the policy. Older protocols are always rejected.
    pub(crate) const fn check(self, found: u32) -> Result<(), UnsupportedProtocol> {
        let accepted = found == SUPPORTED_PROTOCOL_VERSION
            || (matches!(self, Self::AllowNewer) && found == NEWER_PROTOCOL_VERSION);
        if accepted {
            Ok(())
        } else {
            Err(UnsupportedProtocol {
                found,
                supported: SUPPORTED_PROTOCOL_VERSION,
            })
        }
    }
}

/// Crate-internal carrier for the fields of the public `UnsupportedProtocol`
/// error variants.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UnsupportedProtocol {
    pub(crate) found: u32,
    pub(crate) supported: u32,
}

impl From<UnsupportedProtocol> for FixtureError {
    fn from(error: UnsupportedProtocol) -> Self {
        Self::UnsupportedProtocol {
            found: error.found,
            supported: error.supported,
        }
    }
}

/// Ledger protocol handed to the selected Host for a validated network ledger
/// protocol.
///
/// Only a ledger exactly one protocol ahead, which is accepted solely through
/// the opt-in newer-protocol mode, is executed as
/// [`SUPPORTED_PROTOCOL_VERSION`]. Every other value is passed through
/// unchanged, so an unvalidated newer ledger still reaches the Host's own
/// fail-closed protocol check instead of being silently downgraded.
pub(crate) const fn executed_protocol_version(network_protocol_version: u32) -> u32 {
    if network_protocol_version == NEWER_PROTOCOL_VERSION {
        SUPPORTED_PROTOCOL_VERSION
    } else {
        network_protocol_version
    }
}

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
        Self::from_file_with_policy(path, expected_network_passphrase, ProtocolPolicy::Exact)
    }

    /// Like [`Self::from_file`], but also accepts a ledger exactly one
    /// protocol newer than [`SUPPORTED_PROTOCOL_VERSION`].
    ///
    /// See [`Self::from_snapshot_allowing_newer_protocol`] for what this mode
    /// changes and what it keeps.
    ///
    /// # Errors
    ///
    /// Returns the errors documented by [`Self::from_file`]. Protocols older
    /// than [`SUPPORTED_PROTOCOL_VERSION`] and protocols two or more above it
    /// are still rejected with [`FixtureError::UnsupportedProtocol`].
    pub fn from_file_allowing_newer_protocol(
        path: impl AsRef<Path>,
        expected_network_passphrase: &str,
    ) -> Result<Self, FixtureError> {
        Self::from_file_with_policy(
            path,
            expected_network_passphrase,
            ProtocolPolicy::AllowNewer,
        )
    }

    fn from_file_with_policy(
        path: impl AsRef<Path>,
        expected_network_passphrase: &str,
        policy: ProtocolPolicy,
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
        Self::from_snapshot_with_policy(snapshot, expected_network_passphrase, policy)
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
        Self::from_snapshot_with_policy(
            snapshot,
            expected_network_passphrase,
            ProtocolPolicy::Exact,
        )
    }

    /// Like [`Self::from_snapshot`], but also accepts a ledger exactly one
    /// protocol newer than [`SUPPORTED_PROTOCOL_VERSION`].
    ///
    /// The fixture stays truthful: its snapshot, protocol field, and
    /// [`Self::ledger_digest`] keep the real network protocol. Only execution
    /// is downgraded: every `Env` created from it gives the Host
    /// [`SUPPORTED_PROTOCOL_VERSION`] as the ledger protocol, reported by
    /// [`Self::executed_protocol_version`]. Host behaviour that changed in the
    /// newer protocol without an XDR change, such as metering, therefore
    /// follows the older Host. Entries the older XDR cannot decode, and
    /// contracts built for a newer interface version, still fail closed.
    ///
    /// # Errors
    ///
    /// Returns the errors documented by [`Self::from_snapshot`]. Protocols
    /// older than [`SUPPORTED_PROTOCOL_VERSION`] and protocols two or more
    /// above it are still rejected with [`FixtureError::UnsupportedProtocol`].
    pub fn from_snapshot_allowing_newer_protocol(
        snapshot: LedgerSnapshot,
        expected_network_passphrase: &str,
    ) -> Result<Self, FixtureError> {
        Self::from_snapshot_with_policy(
            snapshot,
            expected_network_passphrase,
            ProtocolPolicy::AllowNewer,
        )
    }

    pub(crate) fn from_snapshot_with_policy(
        snapshot: LedgerSnapshot,
        expected_network_passphrase: &str,
        policy: ProtocolPolicy,
    ) -> Result<Self, FixtureError> {
        policy.check(snapshot.protocol_version)?;

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
    ///
    /// Its `protocol_version` is always the network protocol the ledger was
    /// recorded at, also under the newer-protocol mode.
    #[must_use]
    pub const fn ledger_snapshot(&self) -> &LedgerSnapshot {
        &self.snapshot
    }

    /// Ledger protocol the Host executes for this fixture.
    ///
    /// Equals the snapshot's network protocol, except for a ledger one
    /// protocol newer than [`SUPPORTED_PROTOCOL_VERSION`] accepted through
    /// [`Self::from_snapshot_allowing_newer_protocol`] or
    /// [`Self::from_file_allowing_newer_protocol`], which executes as
    /// [`SUPPORTED_PROTOCOL_VERSION`].
    #[must_use]
    pub const fn executed_protocol_version(&self) -> u32 {
        executed_protocol_version(self.snapshot.protocol_version)
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
