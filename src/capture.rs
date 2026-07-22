//! Address-first capture of the ledger state a Soroban scenario actually uses.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    ConfigSettingEntry, ConfigSettingId, ContractDataDurability, ContractExecutable, Hash,
    LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerHeader, LedgerKey, LedgerKeyConfigSetting,
    LedgerKeyContractCode, LedgerKeyContractData, Limits, ReadXdr, ScAddress, ScErrorCode,
    ScErrorType, ScVal, StateArchivalSettings, WriteXdr,
};
use soroban_ledger_snapshot::LedgerSnapshot;
use soroban_sdk::{
    testutils::{EnvTestConfig, HostError, LedgerInfo, SnapshotSource, SnapshotSourceInput},
    Address, Env, TryFromVal,
};
use thiserror::Error;

use crate::{FixtureError, Fork, FrozenFixture, SUPPORTED_PROTOCOL_VERSION};

const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const MAX_COHERENCE_ATTEMPTS: usize = 8;
const MAX_DISCOVERY_ROUNDS: usize = 16;
const MAX_KEYS_PER_BATCH: usize = 200;
const CAPTURE_BUNDLE_SCHEMA_VERSION: u32 = 1;
const INVENTORY_DIGEST_DOMAIN_V1: &[u8] = b"KANATOKO\0CAPTURE-INVENTORY\0V1\0";
const BUNDLE_DIGEST_DOMAIN_V1: &[u8] = b"KANATOKO\0CAPTURE-BUNDLE\0V1\0";
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

type KeyId = Vec<u8>;
type PresentEntry = (Rc<LedgerEntry>, Option<u32>);

/// Builds an address-first, RPC-backed capture.
///
/// The URL is held only by the HTTP transport. Debug output and capture
/// provenance retain only the scheme and hostname/port. Userinfo, path, query,
/// and fragment are discarded. A credential embedded in the hostname cannot
/// be recognized and will remain visible, so credentials must not be placed
/// there.
pub struct CaptureBuilder {
    root_contract: String,
    network_passphrase: String,
    rpc_origin: String,
    transport: Rc<dyn Transport>,
    max_coherence_attempts: usize,
    max_discovery_rounds: usize,
}

impl CaptureBuilder {
    /// Creates a capture builder for Stellar public mainnet.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL has no HTTP(S) origin. The contract ID is
    /// validated before the first RPC read by [`Self::capture`].
    pub fn mainnet(
        rpc_url: impl Into<String>,
        root_contract: impl Into<String>,
    ) -> Result<Self, CaptureError> {
        Self::rpc(rpc_url, MAINNET_PASSPHRASE, root_contract)
    }

    /// Creates a capture builder for an explicit RPC network and passphrase.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL has no HTTP(S) origin.
    pub fn rpc(
        rpc_url: impl Into<String>,
        network_passphrase: impl Into<String>,
        root_contract: impl Into<String>,
    ) -> Result<Self, CaptureError> {
        let rpc_url = rpc_url.into();
        let rpc_origin = redact_rpc_origin(&rpc_url)?;
        let transport = Rc::new(HttpTransport::new(rpc_url));
        Ok(Self {
            root_contract: root_contract.into(),
            network_passphrase: network_passphrase.into(),
            rpc_origin,
            transport,
            max_coherence_attempts: MAX_COHERENCE_ATTEMPTS,
            max_discovery_rounds: MAX_DISCOVERY_ROUNDS,
        })
    }

    /// Captures the fixed point of ledger keys touched by `scenario`.
    ///
    /// A fresh [`Env`] and root [`Address`] are supplied on every run, so
    /// generated clients must be recreated inside the closure. The closure can
    /// run several times and must be deterministic, repeatable, and free of
    /// external side effects. Scenario panics are converted to a terminal
    /// error only after key discovery stops; Rust's standard panic hook still
    /// runs and may print the panic payload, so panic messages must not contain
    /// secrets.
    ///
    /// # Errors
    ///
    /// Fails closed on network/protocol mismatch, transport or XDR failure,
    /// incoherent ledger batches, a missing root/code entry, a terminal
    /// scenario panic, or a bounded fixed-point failure.
    pub fn capture<F>(&self, scenario: F) -> Result<CapturedFixture, CaptureError>
    where
        F: Fn(&Env, &Address),
    {
        self.capture_ref(&scenario)
    }

    fn capture_ref<F>(&self, scenario: &F) -> Result<CapturedFixture, CaptureError>
    where
        F: Fn(&Env, &Address),
    {
        let root = parse_contract_address(&self.root_contract)?;
        let network = self
            .transport
            .network()
            .map_err(|failure| transport_error(failure.operation))?;
        if network.passphrase != self.network_passphrase {
            return Err(CaptureError::NetworkMismatch);
        }
        if network.protocol_version != SUPPORTED_PROTOCOL_VERSION {
            return Err(CaptureError::UnsupportedProtocol {
                found: network.protocol_version,
                supported: SUPPORTED_PROTOCOL_VERSION,
            });
        }

        let mut keys = BTreeMap::new();
        insert_key(&mut keys, contract_instance_key(root.clone()))?;
        let mut materialized = self.materialize(&keys)?;
        ensure_root_present(&root, &materialized.coverage)?;
        materialized = self.expand_code_closure(&mut keys, materialized)?;

        for round in 1..=self.max_discovery_rounds {
            let source = Rc::new(TrackingSource::rpc(
                materialized.coverage.clone(),
                self.transport.clone(),
            ));
            let env = env_from_materialized(&materialized, source.clone());
            let root_address = Address::try_from_val(&env, &root)
                .map_err(|_| CaptureError::InvalidRootContract)?;
            let outcome = catch_unwind(AssertUnwindSafe(|| scenario(&env, &root_address)));

            if let Some(operation) = source.take_failure() {
                return Err(transport_error(operation));
            }

            let mut requested = source.requested_keys();
            for (key, _) in env
                .host()
                .get_stored_entries()
                .map_err(|_| CaptureError::HostInspection)?
            {
                let key = (*key).clone();
                requested.insert(key_id(&key)?, key);
            }

            let mut added = 0_usize;
            for (_, key) in requested {
                if is_state_archival_key(&key) {
                    continue;
                }
                let id = key_id(&key)?;
                if keys.insert(id, key).is_none() {
                    added += 1;
                }
            }

            if added != 0 {
                materialized = self.materialize(&keys)?;
                materialized = self.expand_code_closure(&mut keys, materialized)?;
                continue;
            }

            if outcome.is_err() {
                return Err(CaptureError::ScenarioPanicked);
            }
            if source.rpc_reads() != 0 {
                return Err(CaptureError::InternalInvariant);
            }

            let snapshot = snapshot_from_materialized(&materialized)?;
            let fixture = FrozenFixture::from_snapshot(snapshot, &self.network_passphrase)?;
            let inventory_digest = inventory_digest(&materialized.coverage)?;
            let present_entries = materialized
                .coverage
                .values()
                .filter(|state| matches!(state, LookupState::Present(_, _)))
                .count();
            let absent_entries = materialized.coverage.len() - present_entries;
            let report = CaptureReport {
                discovery_rounds: round,
                present_entries,
                absent_entries,
                inventory_digest,
                final_replay_rpc_reads: 0,
            };
            let provenance = CaptureProvenance {
                rpc_origin: self.rpc_origin.clone(),
                network_passphrase: self.network_passphrase.clone(),
                ledger_sequence: materialized.anchor.sequence,
                ledger_hash: materialized.anchor.hash.clone(),
                protocol_version: materialized.anchor.protocol_version,
            };
            return Ok(CapturedFixture {
                fixture,
                root_contract: self.root_contract.clone(),
                root,
                coverage: materialized.coverage,
                report,
                provenance,
            });
        }

        Err(CaptureError::FixedPointLimit {
            rounds: self.max_discovery_rounds,
        })
    }

    fn materialize(&self, keys: &BTreeMap<KeyId, LedgerKey>) -> Result<Materialized, CaptureError> {
        let config_key = state_archival_key();
        let config_id = key_id(&config_key)?;
        let mut requested = keys.clone();
        requested.insert(config_id.clone(), config_key);
        let requested: Vec<(KeyId, LedgerKey)> = requested.into_iter().collect();

        for _ in 0..self.max_coherence_attempts {
            let before = self
                .transport
                .latest_ledger()
                .map_err(|failure| transport_error(failure.operation))?;
            if before.protocol_version != SUPPORTED_PROTOCOL_VERSION {
                return Err(CaptureError::UnsupportedProtocol {
                    found: before.protocol_version,
                    supported: SUPPORTED_PROTOCOL_VERSION,
                });
            }

            let mut coverage = BTreeMap::new();
            let mut archival = None;
            let mut batches_match = true;
            for chunk in requested.chunks(MAX_KEYS_PER_BATCH) {
                let batch_keys: Vec<LedgerKey> = chunk.iter().map(|(_, key)| key.clone()).collect();
                let batch = self
                    .transport
                    .ledger_entries(&batch_keys)
                    .map_err(|failure| transport_error(failure.operation))?;
                if batch.latest_ledger != before.sequence {
                    batches_match = false;
                    break;
                }
                let returned = validate_batch(&batch_keys, batch.entries)?;
                for (id, key) in chunk {
                    let value = returned.get(id).cloned();
                    if id == &config_id {
                        archival = Some(extract_archival(value.as_ref())?);
                    } else {
                        coverage.insert(
                            id.clone(),
                            value.map_or(LookupState::Absent, |entry| {
                                LookupState::Present(entry.entry, entry.live_until)
                            }),
                        );
                    }
                    debug_assert_eq!(id, &key_id(key)?);
                }
            }

            let after = self
                .transport
                .latest_ledger()
                .map_err(|failure| transport_error(failure.operation))?;
            if !batches_match || before != after {
                continue;
            }
            let archival = archival.ok_or(CaptureError::MissingStateArchival)?;
            validate_coverage(&coverage, before.sequence, archival.max_entry_ttl)?;
            let ledger_info = LedgerInfo {
                protocol_version: before.protocol_version,
                sequence_number: before.sequence,
                timestamp: before.timestamp,
                network_id: Sha256::digest(self.network_passphrase.as_bytes()).into(),
                base_reserve: before.base_reserve,
                min_persistent_entry_ttl: archival.min_persistent_ttl,
                min_temp_entry_ttl: archival.min_temporary_ttl,
                max_entry_ttl: archival.max_entry_ttl,
            };
            return Ok(Materialized {
                anchor: before,
                ledger_info,
                coverage,
            });
        }

        Err(CaptureError::IncoherentLedger {
            attempts: self.max_coherence_attempts,
        })
    }

    fn expand_code_closure(
        &self,
        keys: &mut BTreeMap<KeyId, LedgerKey>,
        mut materialized: Materialized,
    ) -> Result<Materialized, CaptureError> {
        loop {
            let mut added = false;
            for state in materialized.coverage.values() {
                let LookupState::Present(entry, _) = state else {
                    continue;
                };
                let LedgerEntryData::ContractData(data) = &entry.data else {
                    continue;
                };
                let ScVal::ContractInstance(instance) = &data.val else {
                    continue;
                };
                if let ContractExecutable::Wasm(hash) = &instance.executable {
                    let code_key =
                        LedgerKey::ContractCode(LedgerKeyContractCode { hash: hash.clone() });
                    let id = key_id(&code_key)?;
                    if keys.insert(id, code_key).is_none() {
                        added = true;
                    }
                }
            }
            if !added {
                ensure_referenced_code_present(&materialized.coverage)?;
                return Ok(materialized);
            }
            materialized = self.materialize(keys)?;
        }
    }

    #[cfg(test)]
    fn with_transport(
        transport: Rc<dyn Transport>,
        network_passphrase: impl Into<String>,
        root_contract: impl Into<String>,
    ) -> Self {
        Self {
            root_contract: root_contract.into(),
            network_passphrase: network_passphrase.into(),
            rpc_origin: "https://fake.invalid".to_string(),
            transport,
            max_coherence_attempts: MAX_COHERENCE_ATTEMPTS,
            max_discovery_rounds: MAX_DISCOVERY_ROUNDS,
        }
    }
}

impl fmt::Debug for CaptureBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureBuilder")
            .field("root_contract", &self.root_contract)
            .field("rpc_origin", &self.rpc_origin)
            .field("max_coherence_attempts", &self.max_coherence_attempts)
            .field("max_discovery_rounds", &self.max_discovery_rounds)
            .finish_non_exhaustive()
    }
}

/// A coherent starting state plus strict Present/Absent coverage.
#[derive(Clone, Debug)]
pub struct CapturedFixture {
    fixture: FrozenFixture,
    root_contract: String,
    root: ScAddress,
    coverage: BTreeMap<KeyId, LookupState>,
    report: CaptureReport,
    provenance: CaptureProvenance,
}

impl CapturedFixture {
    /// Returns the validated frozen ledger snapshot.
    #[must_use]
    pub const fn frozen_fixture(&self) -> &FrozenFixture {
        &self.fixture
    }

    /// Returns the root contract `StrKey` supplied to the builder.
    #[must_use]
    pub fn root_contract(&self) -> &str {
        &self.root_contract
    }

    /// Returns capture counts, fixed-point rounds, and inventory digest.
    #[must_use]
    pub const fn report(&self) -> &CaptureReport {
        &self.report
    }

    /// Returns capture provenance with RPC userinfo/path/query removed.
    ///
    /// The hostname is retained verbatim and must not contain credentials.
    #[must_use]
    pub const fn provenance(&self) -> &CaptureProvenance {
        &self.provenance
    }

    /// Iterates canonical XDR keys proven absent at the captured ledger.
    pub fn known_absent_keys(&self) -> impl Iterator<Item = &[u8]> {
        self.coverage.iter().filter_map(|(key, state)| {
            matches!(state, LookupState::Absent).then_some(key.as_slice())
        })
    }

    /// Atomically writes a versioned, self-validating capture bundle.
    ///
    /// The temporary file is created and synced in the destination directory,
    /// then renamed over `path`. The legacy M0/M1 ledger snapshot format is not
    /// changed by this API.
    ///
    /// # Errors
    ///
    /// Returns an error when the bundle cannot be encoded, written, synced, or
    /// atomically renamed.
    pub fn write_file(&self, path: impl AsRef<Path>) -> Result<(), CaptureError> {
        let bundle = self.to_bundle()?;
        let bytes =
            serde_json::to_vec_pretty(&bundle).map_err(|_| CaptureError::MalformedCaptureBundle)?;
        atomic_write(path.as_ref(), &bytes)
    }

    /// Reads and validates a versioned capture bundle before constructing an
    /// offline replay fixture.
    ///
    /// # Errors
    ///
    /// Fails closed on I/O or JSON errors, an unsupported schema, a network or
    /// protocol mismatch, malformed XDR, changed canonical digests, inconsistent
    /// Present/Absent coverage, or a missing root instance/referenced WASM.
    pub fn from_file(
        path: impl AsRef<Path>,
        expected_network_passphrase: &str,
    ) -> Result<Self, CaptureError> {
        let bytes = fs::read(path.as_ref()).map_err(|source| CaptureError::CaptureBundleIo {
            operation: "read",
            source,
        })?;
        let bundle: CaptureBundleV1 =
            serde_json::from_slice(&bytes).map_err(|_| CaptureError::MalformedCaptureBundle)?;
        Self::from_bundle(bundle, expected_network_passphrase)
    }

    /// Replays once with strict unknown-key rejection and no RPC transport.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::UnknownLedgerKeys`] if the scenario touches a
    /// key outside captured Present/Absent coverage, or
    /// [`CaptureError::ScenarioPanicked`] for any other contained panic. The
    /// standard Rust panic hook still runs and can print the payload; never put
    /// secrets in scenario panic messages.
    pub fn replay<F, R>(&self, scenario: F) -> Result<R, CaptureError>
    where
        F: FnOnce(&Env, &Address) -> R,
    {
        let materialized = Materialized {
            anchor: Anchor {
                sequence: self.fixture.ledger_snapshot().sequence_number,
                hash: self.provenance.ledger_hash.clone(),
                protocol_version: self.fixture.ledger_snapshot().protocol_version,
                timestamp: self.fixture.ledger_snapshot().timestamp,
                base_reserve: self.fixture.ledger_snapshot().base_reserve,
            },
            ledger_info: self.fixture.ledger_snapshot().ledger_info(),
            coverage: self.coverage.clone(),
        };
        let source = Rc::new(TrackingSource::strict(self.coverage.clone()));
        let env = env_from_materialized(&materialized, source.clone());
        let root = Address::try_from_val(&env, &self.root)
            .map_err(|_| CaptureError::InvalidRootContract)?;
        let outcome = catch_unwind(AssertUnwindSafe(|| scenario(&env, &root)));

        let mut unknown = source.unknown_keys();
        for (key, _) in env
            .host()
            .get_stored_entries()
            .map_err(|_| CaptureError::HostInspection)?
        {
            let id = key_id(&key)?;
            if !self.coverage.contains_key(&id) {
                unknown.insert(id);
            }
        }
        if !unknown.is_empty() {
            return Err(CaptureError::UnknownLedgerKeys {
                count: unknown.len(),
            });
        }
        outcome.map_err(|_| CaptureError::ScenarioPanicked)
    }

    /// Creates the legacy M0 fork view.
    ///
    /// Unlike [`Self::replay`], M0 [`Fork`] semantics treat uncaptured keys as
    /// missing and do not preserve strict Unknown versus confirmed-Absent.
    #[must_use]
    pub fn fork(&self) -> Fork {
        Fork::from_fixture(&self.fixture)
    }

    fn to_bundle(&self) -> Result<CaptureBundleV1, CaptureError> {
        let known_absent: Vec<String> = self
            .known_absent_keys()
            .map(|key| BASE64.encode(key))
            .collect();
        let ledger_digest = self.fixture.ledger_digest();
        let inventory_digest = inventory_digest(&self.coverage)?;
        if inventory_digest != self.report.inventory_digest {
            return Err(CaptureError::InternalInvariant);
        }
        let report = BundleReport {
            discovery_rounds: u32::try_from(self.report.discovery_rounds)
                .map_err(|_| CaptureError::InternalInvariant)?,
            present_entries: u64::try_from(self.report.present_entries)
                .map_err(|_| CaptureError::InternalInvariant)?,
            absent_entries: u64::try_from(self.report.absent_entries)
                .map_err(|_| CaptureError::InternalInvariant)?,
            final_replay_rpc_reads: self.report.final_replay_rpc_reads,
        };
        let provenance = BundleProvenance {
            rpc_origin: self.provenance.rpc_origin.clone(),
            network_passphrase: self.provenance.network_passphrase.clone(),
            ledger_sequence: self.provenance.ledger_sequence,
            ledger_hash: self.provenance.ledger_hash.clone(),
            protocol_version: self.provenance.protocol_version,
        };
        let canonical_digest = canonical_bundle_digest(
            &self.root_contract,
            &known_absent,
            &report,
            &provenance,
            &ledger_digest,
            &inventory_digest,
        )?;
        Ok(CaptureBundleV1 {
            schema_version: CAPTURE_BUNDLE_SCHEMA_VERSION,
            root_contract: self.root_contract.clone(),
            ledger_snapshot: self.fixture.ledger_snapshot().clone(),
            known_absent,
            report,
            provenance,
            ledger_digest: hex(ledger_digest),
            inventory_digest: hex(inventory_digest),
            canonical_digest: hex(canonical_digest),
        })
    }

    fn from_bundle(
        bundle: CaptureBundleV1,
        expected_network_passphrase: &str,
    ) -> Result<Self, CaptureError> {
        let digests = validate_bundle_envelope(&bundle, expected_network_passphrase)?;
        let fixture =
            FrozenFixture::from_snapshot(bundle.ledger_snapshot, expected_network_passphrase)?;
        if fixture.ledger_digest() != digests.ledger {
            return Err(CaptureError::CaptureBundleIntegrity);
        }

        let root = parse_contract_address(&bundle.root_contract)?;
        let coverage = bundle_coverage(fixture.ledger_snapshot(), &bundle.known_absent)?;
        let (present_entries, absent_entries) = validate_bundle_coverage(
            &coverage,
            &root,
            &bundle.report,
            &digests.inventory,
            fixture.ledger_snapshot().sequence_number,
            fixture.ledger_snapshot().max_entry_ttl,
        )?;

        let report = CaptureReport {
            discovery_rounds: usize::try_from(bundle.report.discovery_rounds)
                .map_err(|_| CaptureError::MalformedCaptureBundle)?,
            present_entries,
            absent_entries,
            inventory_digest: digests.inventory,
            final_replay_rpc_reads: 0,
        };
        let provenance = CaptureProvenance {
            rpc_origin: bundle.provenance.rpc_origin,
            network_passphrase: bundle.provenance.network_passphrase,
            ledger_sequence: bundle.provenance.ledger_sequence,
            ledger_hash: bundle.provenance.ledger_hash,
            protocol_version: bundle.provenance.protocol_version,
        };
        Ok(Self {
            fixture,
            root_contract: bundle.root_contract,
            root,
            coverage,
            report,
            provenance,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureBundleV1 {
    schema_version: u32,
    root_contract: String,
    ledger_snapshot: LedgerSnapshot,
    known_absent: Vec<String>,
    report: BundleReport,
    provenance: BundleProvenance,
    ledger_digest: String,
    inventory_digest: String,
    canonical_digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleReport {
    discovery_rounds: u32,
    present_entries: u64,
    absent_entries: u64,
    final_replay_rpc_reads: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleProvenance {
    rpc_origin: String,
    network_passphrase: String,
    ledger_sequence: u32,
    ledger_hash: String,
    protocol_version: u32,
}

struct BundleDigests {
    ledger: [u8; 32],
    inventory: [u8; 32],
}

fn validate_bundle_envelope(
    bundle: &CaptureBundleV1,
    expected_network_passphrase: &str,
) -> Result<BundleDigests, CaptureError> {
    if bundle.schema_version != CAPTURE_BUNDLE_SCHEMA_VERSION {
        return Err(CaptureError::UnsupportedCaptureBundleSchema {
            found: bundle.schema_version,
            supported: CAPTURE_BUNDLE_SCHEMA_VERSION,
        });
    }
    if bundle.provenance.network_passphrase != expected_network_passphrase {
        return Err(CaptureError::NetworkMismatch);
    }
    if redact_rpc_origin(&bundle.provenance.rpc_origin)? != bundle.provenance.rpc_origin {
        return Err(CaptureError::MalformedCaptureBundle);
    }
    let discovery_rounds = usize::try_from(bundle.report.discovery_rounds)
        .map_err(|_| CaptureError::MalformedCaptureBundle)?;
    if bundle.provenance.ledger_sequence != bundle.ledger_snapshot.sequence_number
        || bundle.provenance.protocol_version != bundle.ledger_snapshot.protocol_version
        || bundle.provenance.ledger_hash.is_empty()
        || discovery_rounds == 0
        || discovery_rounds > MAX_DISCOVERY_ROUNDS
        || bundle.report.final_replay_rpc_reads != 0
    {
        return Err(CaptureError::CaptureBundleIntegrity);
    }

    let ledger = decode_hex_digest(&bundle.ledger_digest)?;
    let inventory = decode_hex_digest(&bundle.inventory_digest)?;
    let actual_canonical = decode_hex_digest(&bundle.canonical_digest)?;
    let expected_canonical = canonical_bundle_digest(
        &bundle.root_contract,
        &bundle.known_absent,
        &bundle.report,
        &bundle.provenance,
        &ledger,
        &inventory,
    )?;
    if actual_canonical != expected_canonical {
        return Err(CaptureError::CaptureBundleIntegrity);
    }
    Ok(BundleDigests { ledger, inventory })
}

fn bundle_coverage(
    snapshot: &LedgerSnapshot,
    known_absent: &[String],
) -> Result<BTreeMap<KeyId, LookupState>, CaptureError> {
    let mut coverage = BTreeMap::new();
    for (_, (entry, live_until)) in &snapshot.ledger_entries {
        let key = entry.to_key();
        if is_state_archival_key(&key) {
            return Err(CaptureError::MalformedCaptureBundle);
        }
        let id = key_id(&key)?;
        if coverage
            .insert(
                id,
                LookupState::Present(Rc::new((**entry).clone()), *live_until),
            )
            .is_some()
        {
            return Err(CaptureError::CaptureBundleIntegrity);
        }
    }

    let mut previous_absent = None;
    for encoded in known_absent {
        let bytes = BASE64
            .decode(encoded)
            .map_err(|_| CaptureError::MalformedCaptureBundle)?;
        if BASE64.encode(&bytes) != *encoded {
            return Err(CaptureError::MalformedCaptureBundle);
        }
        let key = LedgerKey::from_xdr(bytes.clone(), Limits::none())
            .map_err(|_| CaptureError::MalformedCaptureBundle)?;
        if key_id(&key)? != bytes || is_state_archival_key(&key) {
            return Err(CaptureError::MalformedCaptureBundle);
        }
        if previous_absent
            .as_ref()
            .is_some_and(|previous| previous >= &bytes)
        {
            return Err(CaptureError::MalformedCaptureBundle);
        }
        previous_absent = Some(bytes.clone());
        if coverage.insert(bytes, LookupState::Absent).is_some() {
            return Err(CaptureError::CaptureBundleIntegrity);
        }
    }
    Ok(coverage)
}

fn validate_bundle_coverage(
    coverage: &BTreeMap<KeyId, LookupState>,
    root: &ScAddress,
    report: &BundleReport,
    expected_inventory_digest: &[u8; 32],
    ledger_sequence: u32,
    max_entry_ttl: u32,
) -> Result<(usize, usize), CaptureError> {
    if validate_coverage(coverage, ledger_sequence, max_entry_ttl).is_err()
        || ensure_root_present(root, coverage).is_err()
        || ensure_referenced_code_present(coverage).is_err()
    {
        return Err(CaptureError::CaptureBundleIntegrity);
    }
    let present_entries = coverage
        .values()
        .filter(|state| matches!(state, LookupState::Present(_, _)))
        .count();
    let absent_entries = coverage.len() - present_entries;
    if inventory_digest(coverage)? != *expected_inventory_digest
        || u64::try_from(present_entries).ok() != Some(report.present_entries)
        || u64::try_from(absent_entries).ok() != Some(report.absent_entries)
    {
        return Err(CaptureError::CaptureBundleIntegrity);
    }
    Ok((present_entries, absent_entries))
}

/// Stable capture summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureReport {
    discovery_rounds: usize,
    present_entries: usize,
    absent_entries: usize,
    inventory_digest: [u8; 32],
    final_replay_rpc_reads: u64,
}

impl CaptureReport {
    #[must_use]
    pub const fn discovery_rounds(&self) -> usize {
        self.discovery_rounds
    }

    #[must_use]
    pub const fn present_entries(&self) -> usize {
        self.present_entries
    }

    #[must_use]
    pub const fn absent_entries(&self) -> usize {
        self.absent_entries
    }

    #[must_use]
    pub const fn inventory_digest(&self) -> [u8; 32] {
        self.inventory_digest
    }

    #[must_use]
    pub const fn final_replay_rpc_reads(&self) -> u64 {
        self.final_replay_rpc_reads
    }
}

/// Source origin and ledger provenance.
///
/// RPC userinfo/path/query are removed, while hostname/port remain verbatim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureProvenance {
    rpc_origin: String,
    network_passphrase: String,
    ledger_sequence: u32,
    ledger_hash: String,
    protocol_version: u32,
}

impl CaptureProvenance {
    #[must_use]
    pub fn rpc_origin(&self) -> &str {
        &self.rpc_origin
    }

    #[must_use]
    pub fn network_passphrase(&self) -> &str {
        &self.network_passphrase
    }

    #[must_use]
    pub const fn ledger_sequence(&self) -> u32 {
        self.ledger_sequence
    }

    #[must_use]
    pub fn ledger_hash(&self) -> &str {
        &self.ledger_hash
    }

    #[must_use]
    pub const fn protocol_version(&self) -> u32 {
        self.protocol_version
    }
}

/// Fail-closed capture and strict replay errors.
#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("RPC URL must contain an http(s) origin")]
    InvalidRpcUrl,
    #[error("root address is not a contract StrKey")]
    InvalidRootContract,
    #[error("RPC network passphrase does not match the requested network")]
    NetworkMismatch,
    #[error("unsupported ledger protocol {found}; pinned Host supports exactly {supported}")]
    UnsupportedProtocol { found: u32, supported: u32 },
    #[error("RPC transport failed during {operation}")]
    Transport { operation: &'static str },
    #[error("RPC returned malformed data during {operation}")]
    MalformedRpc { operation: &'static str },
    #[error("could not materialize one coherent ledger after {attempts} attempts")]
    IncoherentLedger { attempts: usize },
    #[error("StateArchival config was absent from a coherent ledger")]
    MissingStateArchival,
    #[error("root contract instance is absent from the captured ledger")]
    MissingRootContract,
    #[error("captured contract instance references absent WASM code")]
    MissingReferencedCode,
    #[error("scenario panicked after reaching a ledger-key fixed point")]
    ScenarioPanicked,
    #[error("scenario did not reach a ledger-key fixed point in {rounds} rounds")]
    FixedPointLimit { rounds: usize },
    #[error("strict replay touched {count} unknown ledger key(s)")]
    UnknownLedgerKeys { count: usize },
    #[error("capture bundle I/O failed during {operation}")]
    CaptureBundleIo {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("capture bundle schema {found} is unsupported; expected {supported}")]
    UnsupportedCaptureBundleSchema { found: u32, supported: u32 },
    #[error("capture bundle is malformed")]
    MalformedCaptureBundle,
    #[error("capture bundle integrity validation failed")]
    CaptureBundleIntegrity,
    #[error("Host ledger inspection failed")]
    HostInspection,
    #[error("capture internal invariant failed")]
    InternalInvariant,
    #[error(transparent)]
    Fixture(#[from] FixtureError),
    #[error("ledger XDR encoding failed")]
    Xdr,
}

#[derive(Clone, Debug)]
enum LookupState {
    Present(Rc<LedgerEntry>, Option<u32>),
    Absent,
}

#[derive(Clone)]
struct Materialized {
    anchor: Anchor,
    ledger_info: LedgerInfo,
    coverage: BTreeMap<KeyId, LookupState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Anchor {
    sequence: u32,
    hash: String,
    protocol_version: u32,
    timestamp: u64,
    base_reserve: u32,
}

#[derive(Clone)]
struct Network {
    passphrase: String,
    protocol_version: u32,
}

#[derive(Clone)]
struct FetchedEntry {
    entry: Rc<LedgerEntry>,
    live_until: Option<u32>,
}

struct EntryBatch {
    latest_ledger: u32,
    entries: Vec<FetchedEntry>,
}

#[derive(Clone, Copy, Debug)]
struct TransportFailure {
    operation: &'static str,
}

trait Transport {
    fn network(&self) -> Result<Network, TransportFailure>;
    fn latest_ledger(&self) -> Result<Anchor, TransportFailure>;
    fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<EntryBatch, TransportFailure>;
}

struct TrackingSource {
    cache: RefCell<BTreeMap<KeyId, LookupState>>,
    requested: RefCell<BTreeMap<KeyId, LedgerKey>>,
    unknown: RefCell<BTreeSet<KeyId>>,
    transport: Option<Rc<dyn Transport>>,
    rpc_reads: Cell<u64>,
    failure: Cell<Option<&'static str>>,
}

impl TrackingSource {
    fn rpc(cache: BTreeMap<KeyId, LookupState>, transport: Rc<dyn Transport>) -> Self {
        Self {
            cache: RefCell::new(cache),
            requested: RefCell::new(BTreeMap::new()),
            unknown: RefCell::new(BTreeSet::new()),
            transport: Some(transport),
            rpc_reads: Cell::new(0),
            failure: Cell::new(None),
        }
    }

    fn strict(cache: BTreeMap<KeyId, LookupState>) -> Self {
        Self {
            cache: RefCell::new(cache),
            requested: RefCell::new(BTreeMap::new()),
            unknown: RefCell::new(BTreeSet::new()),
            transport: None,
            rpc_reads: Cell::new(0),
            failure: Cell::new(None),
        }
    }

    fn requested_keys(&self) -> BTreeMap<KeyId, LedgerKey> {
        self.requested.borrow().clone()
    }

    fn unknown_keys(&self) -> BTreeSet<KeyId> {
        self.unknown.borrow().clone()
    }

    fn rpc_reads(&self) -> u64 {
        self.rpc_reads.get()
    }

    fn take_failure(&self) -> Option<&'static str> {
        self.failure.take()
    }

    fn host_error() -> HostError {
        HostError::from((ScErrorType::Storage, ScErrorCode::InternalError))
    }
}

impl SnapshotSource for TrackingSource {
    fn get(&self, key: &Rc<LedgerKey>) -> Result<Option<PresentEntry>, HostError> {
        let Ok(id) = key_id(key) else {
            self.failure.set(Some("ledger-key-encode"));
            return Err(Self::host_error());
        };
        self.requested
            .borrow_mut()
            .insert(id.clone(), (**key).clone());
        if let Some(state) = self.cache.borrow().get(&id).cloned() {
            return Ok(match state {
                LookupState::Present(entry, live_until) => Some((entry, live_until)),
                LookupState::Absent => None,
            });
        }

        let Some(transport) = &self.transport else {
            self.unknown.borrow_mut().insert(id);
            return Err(Self::host_error());
        };
        self.rpc_reads.set(self.rpc_reads.get() + 1);
        let batch = match transport.ledger_entries(&[(**key).clone()]) {
            Ok(batch) => batch,
            Err(failure) => {
                self.failure.set(Some(failure.operation));
                return Err(Self::host_error());
            }
        };
        let Ok(returned) = validate_batch(&[(**key).clone()], batch.entries) else {
            self.failure.set(Some("getLedgerEntries"));
            return Err(Self::host_error());
        };
        let state = returned
            .get(&id)
            .cloned()
            .map_or(LookupState::Absent, |entry| {
                LookupState::Present(entry.entry, entry.live_until)
            });
        self.cache.borrow_mut().insert(id, state.clone());
        Ok(match state {
            LookupState::Present(entry, live_until) => Some((entry, live_until)),
            LookupState::Absent => None,
        })
    }
}

struct HttpTransport {
    rpc_url: String,
    agent: ureq::Agent,
}

impl HttpTransport {
    fn new(rpc_url: String) -> Self {
        Self {
            rpc_url,
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(30))
                .build(),
        }
    }

    fn rpc<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        params: &serde_json::Value,
    ) -> Result<T, TransportFailure> {
        let response = self
            .agent
            .post(&self.rpc_url)
            .send_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": operation,
                "params": params,
            }))
            .map_err(|_| TransportFailure { operation })?;
        let body: RpcResponse<T> = response
            .into_json()
            .map_err(|_| TransportFailure { operation })?;
        if body.error.is_some() {
            return Err(TransportFailure { operation });
        }
        body.result.ok_or(TransportFailure { operation })
    }
}

impl Transport for HttpTransport {
    fn network(&self) -> Result<Network, TransportFailure> {
        let wire: NetworkWire = self.rpc("getNetwork", &serde_json::json!({}))?;
        Ok(Network {
            passphrase: wire.passphrase,
            protocol_version: wire.protocol_version,
        })
    }

    fn latest_ledger(&self) -> Result<Anchor, TransportFailure> {
        let operation = "getLatestLedger";
        let wire: LatestWire = self.rpc(operation, &serde_json::json!({}))?;
        let bytes = BASE64
            .decode(&wire.header_xdr)
            .map_err(|_| TransportFailure { operation })?;
        let header = LedgerHeader::from_xdr(bytes.clone(), Limits::none())
            .map_err(|_| TransportFailure { operation })?;
        let timestamp = wire
            .close_time
            .parse::<u64>()
            .map_err(|_| TransportFailure { operation })?;
        if header.ledger_seq != wire.sequence
            || header.scp_value.close_time.0 != timestamp
            || hex(Sha256::digest(bytes)) != wire.id
        {
            return Err(TransportFailure { operation });
        }
        Ok(Anchor {
            sequence: wire.sequence,
            hash: wire.id,
            protocol_version: wire.protocol_version,
            timestamp,
            base_reserve: header.base_reserve,
        })
    }

    fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<EntryBatch, TransportFailure> {
        let operation = "getLedgerEntries";
        let encoded: Result<Vec<String>, _> = keys
            .iter()
            .map(|key| key.to_xdr(Limits::none()).map(|xdr| BASE64.encode(xdr)))
            .collect();
        let encoded = encoded.map_err(|_| TransportFailure { operation })?;
        let wire: EntriesWire = self.rpc(operation, &serde_json::json!({ "keys": encoded }))?;
        let entries = wire
            .entries
            .unwrap_or_default()
            .into_iter()
            .map(|entry| decode_wire_entry(entry, operation))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EntryBatch {
            latest_ledger: wire.latest_ledger,
            entries,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NetworkWire {
    passphrase: String,
    protocol_version: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LatestWire {
    id: String,
    protocol_version: u32,
    sequence: u32,
    close_time: String,
    header_xdr: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntriesWire {
    entries: Option<Vec<EntryWire>>,
    latest_ledger: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryWire {
    key: String,
    xdr: String,
    last_modified_ledger_seq: u32,
    live_until_ledger_seq: Option<u32>,
    ext_xdr: Option<String>,
}

#[derive(Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<serde_json::Value>,
}

fn decode_wire_entry(
    wire: EntryWire,
    operation: &'static str,
) -> Result<FetchedEntry, TransportFailure> {
    let key_bytes = BASE64
        .decode(wire.key)
        .map_err(|_| TransportFailure { operation })?;
    let key = LedgerKey::from_xdr(key_bytes, Limits::none())
        .map_err(|_| TransportFailure { operation })?;
    let entry_bytes = BASE64
        .decode(wire.xdr)
        .map_err(|_| TransportFailure { operation })?;
    let data = LedgerEntryData::from_xdr(entry_bytes, Limits::none())
        .map_err(|_| TransportFailure { operation })?;
    let ext = wire
        .ext_xdr
        .ok_or(TransportFailure { operation })
        .and_then(|encoded| {
            BASE64
                .decode(encoded)
                .map_err(|_| TransportFailure { operation })
        })
        .and_then(|bytes| {
            LedgerEntryExt::from_xdr(bytes, Limits::none())
                .map_err(|_| TransportFailure { operation })
        })?;
    let entry = LedgerEntry {
        last_modified_ledger_seq: wire.last_modified_ledger_seq,
        data,
        ext,
    };
    if entry.to_key() != key {
        return Err(TransportFailure { operation });
    }
    Ok(FetchedEntry {
        entry: Rc::new(entry),
        live_until: wire.live_until_ledger_seq,
    })
}

fn validate_batch(
    requested: &[LedgerKey],
    entries: Vec<FetchedEntry>,
) -> Result<BTreeMap<KeyId, FetchedEntry>, CaptureError> {
    let requested: BTreeSet<KeyId> = requested.iter().map(key_id).collect::<Result<_, _>>()?;
    let mut returned = BTreeMap::new();
    for fetched in entries {
        let derived = fetched.entry.to_key();
        let id = key_id(&derived)?;
        if !requested.contains(&id) || returned.insert(id, fetched).is_some() {
            return Err(CaptureError::MalformedRpc {
                operation: "getLedgerEntries",
            });
        }
    }
    Ok(returned)
}

fn extract_archival(entry: Option<&FetchedEntry>) -> Result<StateArchivalSettings, CaptureError> {
    let Some(entry) = entry else {
        return Err(CaptureError::MissingStateArchival);
    };
    match &entry.entry.data {
        LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(settings)) => {
            Ok(settings.clone())
        }
        _ => Err(CaptureError::MalformedRpc {
            operation: "getLedgerEntries",
        }),
    }
}

fn validate_coverage(
    coverage: &BTreeMap<KeyId, LookupState>,
    ledger_sequence: u32,
    max_entry_ttl: u32,
) -> Result<(), CaptureError> {
    for (id, state) in coverage {
        let LookupState::Present(entry, live_until) = state else {
            continue;
        };
        if id != &key_id(&entry.to_key())? {
            return Err(CaptureError::MalformedRpc {
                operation: "getLedgerEntries",
            });
        }
        if let LedgerEntryData::ContractCode(code) = &entry.data {
            let digest: [u8; 32] = Sha256::digest(code.code.as_slice()).into();
            if code.hash != Hash(digest) {
                return Err(CaptureError::MalformedRpc {
                    operation: "getLedgerEntries",
                });
            }
        }
        if !validate_entry_metadata(entry, *live_until, ledger_sequence, max_entry_ttl) {
            return Err(CaptureError::MalformedRpc {
                operation: "getLedgerEntries",
            });
        }
    }
    Ok(())
}

fn validate_entry_metadata(
    entry: &LedgerEntry,
    live_until: Option<u32>,
    ledger_sequence: u32,
    max_entry_ttl: u32,
) -> bool {
    if entry.last_modified_ledger_seq > ledger_sequence || max_entry_ttl == 0 {
        return false;
    }
    let Some(max_live_until) = ledger_sequence.checked_add(max_entry_ttl.saturating_sub(1)) else {
        return false;
    };
    let live_contract_ttl = |ttl: u32| ledger_sequence <= ttl && ttl <= max_live_until;

    match &entry.data {
        LedgerEntryData::ContractCode(_) => {
            live_until.is_some_and(|ttl| ttl == 0 || live_contract_ttl(ttl))
        }
        LedgerEntryData::ContractData(data) => match data.durability {
            ContractDataDurability::Persistent => {
                live_until.is_some_and(|ttl| ttl == 0 || live_contract_ttl(ttl))
            }
            ContractDataDurability::Temporary => live_until.is_some_and(live_contract_ttl),
        },
        LedgerEntryData::Account(_) | LedgerEntryData::Trustline(_) => live_until.is_none(),
        _ => false,
    }
}

fn ensure_root_present(
    root: &ScAddress,
    coverage: &BTreeMap<KeyId, LookupState>,
) -> Result<(), CaptureError> {
    let id = key_id(&contract_instance_key(root.clone()))?;
    match coverage.get(&id) {
        Some(LookupState::Present(entry, _)) => match &entry.data {
            LedgerEntryData::ContractData(data)
                if matches!(data.val, ScVal::ContractInstance(_)) =>
            {
                Ok(())
            }
            _ => Err(CaptureError::MissingRootContract),
        },
        _ => Err(CaptureError::MissingRootContract),
    }
}

fn ensure_referenced_code_present(
    coverage: &BTreeMap<KeyId, LookupState>,
) -> Result<(), CaptureError> {
    for state in coverage.values() {
        let LookupState::Present(entry, _) = state else {
            continue;
        };
        let LedgerEntryData::ContractData(data) = &entry.data else {
            continue;
        };
        let ScVal::ContractInstance(instance) = &data.val else {
            continue;
        };
        let ContractExecutable::Wasm(hash) = &instance.executable else {
            continue;
        };
        let id = key_id(&LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: hash.clone(),
        }))?;
        if !matches!(coverage.get(&id), Some(LookupState::Present(_, _))) {
            return Err(CaptureError::MissingReferencedCode);
        }
    }
    Ok(())
}

fn snapshot_from_materialized(materialized: &Materialized) -> Result<LedgerSnapshot, CaptureError> {
    let mut entries = Vec::new();
    for (id, state) in &materialized.coverage {
        let LookupState::Present(entry, live_until) = state else {
            continue;
        };
        let key = entry.to_key();
        if is_state_archival_key(&key) || id != &key_id(&key)? {
            return Err(CaptureError::InternalInvariant);
        }
        entries.push((Box::new(key), (Box::new((**entry).clone()), *live_until)));
    }
    Ok(LedgerSnapshot {
        protocol_version: materialized.ledger_info.protocol_version,
        sequence_number: materialized.ledger_info.sequence_number,
        timestamp: materialized.ledger_info.timestamp,
        network_id: materialized.ledger_info.network_id,
        base_reserve: materialized.ledger_info.base_reserve,
        min_persistent_entry_ttl: materialized.ledger_info.min_persistent_entry_ttl,
        min_temp_entry_ttl: materialized.ledger_info.min_temp_entry_ttl,
        max_entry_ttl: materialized.ledger_info.max_entry_ttl,
        ledger_entries: entries,
    })
}

fn env_from_materialized(materialized: &Materialized, source: Rc<TrackingSource>) -> Env {
    let snapshot = snapshot_from_materialized(materialized).expect("validated materialization");
    let mut env = Env::from_ledger_snapshot(SnapshotSourceInput {
        source,
        ledger_info: Some(materialized.ledger_info.clone()),
        snapshot: Some(Rc::new(snapshot)),
    });
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    env
}

fn inventory_digest(coverage: &BTreeMap<KeyId, LookupState>) -> Result<[u8; 32], CaptureError> {
    let mut digest = Sha256::new();
    digest.update(INVENTORY_DIGEST_DOMAIN_V1);
    digest.update((coverage.len() as u64).to_be_bytes());
    for (key, state) in coverage {
        digest.update((key.len() as u64).to_be_bytes());
        digest.update(key);
        match state {
            LookupState::Present(entry, live_until) => {
                digest.update([1]);
                let xdr = entry
                    .to_xdr(Limits::none())
                    .map_err(|_| CaptureError::Xdr)?;
                digest.update((xdr.len() as u64).to_be_bytes());
                digest.update(xdr);
                if let Some(live_until) = live_until {
                    digest.update([1]);
                    digest.update(live_until.to_be_bytes());
                } else {
                    digest.update([0]);
                }
            }
            LookupState::Absent => digest.update([0]),
        }
    }
    Ok(digest.finalize().into())
}

fn canonical_bundle_digest(
    root_contract: &str,
    known_absent: &[String],
    report: &BundleReport,
    provenance: &BundleProvenance,
    ledger_digest: &[u8; 32],
    inventory_digest: &[u8; 32],
) -> Result<[u8; 32], CaptureError> {
    let mut digest = Sha256::new();
    digest.update(BUNDLE_DIGEST_DOMAIN_V1);
    digest.update(CAPTURE_BUNDLE_SCHEMA_VERSION.to_be_bytes());
    digest_field(&mut digest, root_contract.as_bytes());
    digest.update((known_absent.len() as u64).to_be_bytes());
    for encoded in known_absent {
        let key = BASE64
            .decode(encoded)
            .map_err(|_| CaptureError::MalformedCaptureBundle)?;
        digest_field(&mut digest, &key);
    }
    digest.update(report.discovery_rounds.to_be_bytes());
    digest.update(report.present_entries.to_be_bytes());
    digest.update(report.absent_entries.to_be_bytes());
    digest.update(report.final_replay_rpc_reads.to_be_bytes());
    digest_field(&mut digest, provenance.rpc_origin.as_bytes());
    digest_field(&mut digest, provenance.network_passphrase.as_bytes());
    digest.update(provenance.ledger_sequence.to_be_bytes());
    digest_field(&mut digest, provenance.ledger_hash.as_bytes());
    digest.update(provenance.protocol_version.to_be_bytes());
    digest.update(ledger_digest);
    digest.update(inventory_digest);
    Ok(digest.finalize().into())
}

fn digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn decode_hex_digest(value: &str) -> Result<[u8; 32], CaptureError> {
    if value.len() != 64 || !value.is_ascii() {
        return Err(CaptureError::MalformedCaptureBundle);
    }
    let mut decoded = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_hex_nibble(pair[0])?;
        let low = decode_hex_nibble(pair[1])?;
        decoded[index] = (high << 4) | low;
    }
    if hex(decoded) != value {
        return Err(CaptureError::MalformedCaptureBundle);
    }
    Ok(decoded)
}

fn decode_hex_nibble(value: u8) -> Result<u8, CaptureError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(CaptureError::MalformedCaptureBundle),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CaptureError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or(CaptureError::MalformedCaptureBundle)?;
    let (mut file, temporary_path) = create_temporary_file(parent, file_name)?;

    if let Err(source) = file.write_all(bytes) {
        drop(file);
        cleanup_temporary_file(&temporary_path);
        return Err(CaptureError::CaptureBundleIo {
            operation: "write",
            source,
        });
    }
    if let Err(source) = file.sync_all() {
        drop(file);
        cleanup_temporary_file(&temporary_path);
        return Err(CaptureError::CaptureBundleIo {
            operation: "sync temporary file",
            source,
        });
    }
    drop(file);
    if let Err(source) = fs::rename(&temporary_path, path) {
        cleanup_temporary_file(&temporary_path);
        return Err(CaptureError::CaptureBundleIo {
            operation: "rename",
            source,
        });
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| CaptureError::CaptureBundleIo {
            operation: "sync destination directory",
            source,
        })
}

fn create_temporary_file(
    parent: &Path,
    file_name: &std::ffi::OsStr,
) -> Result<(File, PathBuf), CaptureError> {
    for _ in 0..32 {
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = file_name.to_os_string();
        temporary_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
        let temporary_path = parent.join(temporary_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((file, temporary_path)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(CaptureError::CaptureBundleIo {
                    operation: "create temporary file",
                    source,
                });
            }
        }
    }
    Err(CaptureError::CaptureBundleIo {
        operation: "create temporary file",
        source: io::Error::new(io::ErrorKind::AlreadyExists, "temporary name exhaustion"),
    })
}

fn cleanup_temporary_file(path: &Path) {
    let _ = fs::remove_file(path);
}

fn parse_contract_address(value: &str) -> Result<ScAddress, CaptureError> {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    let address = catch_unwind(AssertUnwindSafe(|| Address::from_str(&env, value)))
        .map_err(|_| CaptureError::InvalidRootContract)?;
    let address: ScAddress = (&address).into();
    if matches!(address, ScAddress::Contract(_)) {
        Ok(address)
    } else {
        Err(CaptureError::InvalidRootContract)
    }
}

fn insert_key(keys: &mut BTreeMap<KeyId, LedgerKey>, key: LedgerKey) -> Result<bool, CaptureError> {
    Ok(keys.insert(key_id(&key)?, key).is_none())
}

fn key_id(key: &LedgerKey) -> Result<KeyId, CaptureError> {
    key.to_xdr(Limits::none()).map_err(|_| CaptureError::Xdr)
}

fn contract_instance_key(contract: ScAddress) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract,
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    })
}

fn state_archival_key() -> LedgerKey {
    LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::StateArchival,
    })
}

fn is_state_archival_key(key: &LedgerKey) -> bool {
    matches!(
        key,
        LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::StateArchival
        })
    )
}

fn transport_error(operation: &'static str) -> CaptureError {
    CaptureError::Transport { operation }
}

fn redact_rpc_origin(raw: &str) -> Result<String, CaptureError> {
    let (scheme, remainder) = raw.split_once("://").ok_or(CaptureError::InvalidRpcUrl)?;
    if !matches!(scheme, "http" | "https") {
        return Err(CaptureError::InvalidRpcUrl);
    }
    let authority = remainder
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())
        .ok_or(CaptureError::InvalidRpcUrl)?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.is_empty() {
        return Err(CaptureError::InvalidRpcUrl);
    }
    Ok(format!("{scheme}://{host}"))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        use fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("String writes are infallible");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::{BTreeMap, BTreeSet},
        rc::Rc,
    };

    use soroban_env_host::xdr::{
        ContractDataEntry, ExtensionPoint, LedgerEntryData, LedgerEntryExt, LedgerEntryExtensionV1,
        LedgerEntryExtensionV1Ext, LedgerKeyContractData, ScAddress, ScVal, SponsorshipDescriptor,
        StateArchivalSettings,
    };
    use soroban_ledger_snapshot::LedgerSnapshot;
    use soroban_sdk::{
        testutils::{MockAuth, MockAuthInvoke},
        token::StellarAssetClient,
        Address, IntoVal, TryFromVal,
    };

    use super::*;

    mod pool {
        #![allow(clippy::too_many_arguments)]

        soroban_sdk::contractimport!(
            file = "fixtures/mainnet/aquarius-xlm-usdc-cp/pool.wasm",
            sha256 = "ae0da5a84b15805c5c7931ac567a8d1b34be3f26b483993d9ff80cb2c3de9852",
        );
    }

    const POOL_ID: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
    const USDC_ID: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
    const ONE_USDC: u128 = 10_000_000;

    #[test]
    fn root_instance_and_matching_wasm_are_seeded_without_dependency_inputs() {
        let fake = FakeTransport::m1();
        let captured = builder(&fake).capture(|_, _| {}).unwrap();
        let root = parse_contract_address(POOL_ID).unwrap();
        let root_id = key_id(&contract_instance_key(root)).unwrap();

        let root_entry = captured
            .frozen_fixture()
            .ledger_snapshot()
            .ledger_entries
            .iter()
            .find(|(key, _)| key_id(key).unwrap() == root_id)
            .unwrap();
        let LedgerEntryData::ContractData(data) = &root_entry.1 .0.data else {
            panic!("root must be contract data");
        };
        let ScVal::ContractInstance(instance) = &data.val else {
            panic!("root must be a contract instance");
        };
        let ContractExecutable::Wasm(hash) = &instance.executable else {
            panic!("Aquarius root must be WASM");
        };
        let code_id = key_id(&LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: hash.clone(),
        }))
        .unwrap();
        assert!(captured
            .frozen_fixture()
            .ledger_snapshot()
            .ledger_entries
            .iter()
            .any(|(key, _)| key_id(key).unwrap() == code_id));
        assert_eq!(captured.report().final_replay_rpc_reads(), 0);
    }

    #[test]
    fn root_validation_requires_a_contract_instance_value() {
        let fake = FakeTransport::m1();
        let captured = builder(&fake).capture(|_, _| {}).unwrap();
        let root = parse_contract_address(POOL_ID).unwrap();
        let root_id = key_id(&contract_instance_key(root.clone())).unwrap();
        let LookupState::Present(entry, live_until) = &captured.coverage[&root_id] else {
            panic!("root coverage must be present");
        };
        let mut malformed = (**entry).clone();
        let LedgerEntryData::ContractData(data) = &mut malformed.data else {
            panic!("root entry must be contract data");
        };
        data.val = ScVal::U32(7);
        let mut coverage = captured.coverage.clone();
        coverage.insert(
            root_id,
            LookupState::Present(Rc::new(malformed), *live_until),
        );

        assert!(matches!(
            ensure_root_present(&root, &coverage),
            Err(CaptureError::MissingRootContract)
        ));
    }

    #[test]
    fn actual_m1_state_auto_discovers_quote_mint_swap_dependencies_and_replays_offline() {
        let fake = FakeTransport::m1();
        let captured = builder(&fake).capture(aquarius_scenario).unwrap();
        assert!(captured.report().present_entries() >= 9);
        assert!(captured.report().absent_entries() >= 1);
        let reads_before = fake.ledger_entry_reads();
        captured.replay(aquarius_scenario).unwrap();
        assert_eq!(fake.ledger_entry_reads(), reads_before);
    }

    #[test]
    fn versioned_bundle_roundtrips_and_replays_strictly_offline() {
        let fake = FakeTransport::m1();
        let captured = builder(&fake).capture(aquarius_scenario).unwrap();
        let path = test_bundle_path("roundtrip");
        captured.write_file(&path).unwrap();

        let loaded = CapturedFixture::from_file(&path, MAINNET_PASSPHRASE).unwrap();
        assert_eq!(loaded.root_contract(), POOL_ID);
        assert_eq!(loaded.report(), captured.report());
        assert_eq!(loaded.provenance(), captured.provenance());
        assert_eq!(
            loaded.frozen_fixture().ledger_digest(),
            captured.frozen_fixture().ledger_digest()
        );
        let reads_before = fake.ledger_entry_reads();
        loaded.replay(aquarius_scenario).unwrap();
        assert_eq!(fake.ledger_entry_reads(), reads_before);
        assert!(matches!(
            CapturedFixture::from_file(&path, "not the captured network"),
            Err(CaptureError::NetworkMismatch)
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn versioned_bundle_rejects_digest_tampering() {
        let fake = FakeTransport::m1();
        let captured = builder(&fake).capture(aquarius_scenario).unwrap();
        let path = test_bundle_path("tamper");
        captured.write_file(&path).unwrap();

        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        document["ledger_digest"] = serde_json::Value::String("00".repeat(32));
        fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        assert!(matches!(
            CapturedFixture::from_file(&path, MAINNET_PASSPHRASE),
            Err(CaptureError::CaptureBundleIntegrity)
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn capture_and_bundle_loading_reject_invalid_ledger_metadata() {
        let mut fake = FakeTransport::m1();
        let root = parse_contract_address(POOL_ID).unwrap();
        let root_id = key_id(&contract_instance_key(root)).unwrap();
        Rc::get_mut(&mut fake)
            .unwrap()
            .entries
            .get_mut(&root_id)
            .unwrap()
            .live_until = None;
        assert!(matches!(
            builder(&fake).capture(|_, _| {}),
            Err(CaptureError::MalformedRpc { .. })
        ));

        let captured = builder(&FakeTransport::m1()).capture(|_, _| {}).unwrap();
        let mut bundle = captured.to_bundle().unwrap();
        let (_, (_, live_until)) = bundle
            .ledger_snapshot
            .ledger_entries
            .iter_mut()
            .find(|(_, (entry, _))| matches!(entry.data, LedgerEntryData::ContractData(_)))
            .unwrap();
        *live_until = None;
        refresh_bundle_digests(&mut bundle);
        assert!(matches!(
            CapturedFixture::from_bundle(bundle, MAINNET_PASSPHRASE),
            Err(CaptureError::CaptureBundleIntegrity)
        ));
    }

    #[test]
    fn versioned_bundle_roundtrip_preserves_v1_ledger_entry_extension() {
        let captured = builder(&FakeTransport::m1())
            .capture(aquarius_scenario)
            .unwrap();
        let mut bundle = captured.to_bundle().unwrap();
        let account_entry = bundle
            .ledger_snapshot
            .ledger_entries
            .iter_mut()
            .map(|(_, (entry, _))| entry.as_mut())
            .find(|entry| matches!(entry.data, LedgerEntryData::Account(_)))
            .unwrap();
        let LedgerEntryData::Account(account) = &account_entry.data else {
            unreachable!();
        };
        let extension = LedgerEntryExt::V1(LedgerEntryExtensionV1 {
            sponsoring_id: SponsorshipDescriptor(Some(account.account_id.clone())),
            ext: LedgerEntryExtensionV1Ext::V0,
        });
        account_entry.ext = extension.clone();
        refresh_bundle_digests(&mut bundle);

        let loaded = CapturedFixture::from_bundle(bundle, MAINNET_PASSPHRASE).unwrap();
        let loaded_extension = loaded
            .frozen_fixture()
            .ledger_snapshot()
            .ledger_entries
            .iter()
            .map(|(_, (entry, _))| entry.as_ref())
            .find(|entry| matches!(entry.data, LedgerEntryData::Account(_)))
            .unwrap()
            .ext
            .clone();
        assert_eq!(loaded_extension, extension);
    }

    #[test]
    fn write_only_and_confirmed_missing_keys_are_sealed_but_other_unknowns_fail_replay() {
        let fake = FakeTransport::m1();
        let root = parse_contract_address(POOL_ID).unwrap();
        let missing = data_key(root.clone(), 777);
        let write_only = data_key(root.clone(), 778);
        let written_entry = contract_data_entry(root, 778, 42);
        let scenario = |env: &Env, _: &Address| {
            assert!(env
                .host()
                .get_ledger_entry(&Rc::new(missing.clone()))
                .unwrap()
                .is_none());
            env.host()
                .add_ledger_entry(
                    &Rc::new(write_only.clone()),
                    &Rc::new(written_entry.clone()),
                    Some(env.ledger().sequence() + 100),
                )
                .unwrap();
        };

        let captured = builder(&fake).capture(scenario).unwrap();
        let absent: BTreeSet<Vec<u8>> = captured.known_absent_keys().map(<[u8]>::to_vec).collect();
        assert!(absent.contains(&key_id(&missing).unwrap()));
        assert!(absent.contains(&key_id(&write_only).unwrap()));

        let unknown = data_key(parse_contract_address(POOL_ID).unwrap(), 779);
        let error = captured
            .replay(|env, _| {
                let _ = env.host().get_ledger_entry(&Rc::new(unknown));
            })
            .unwrap_err();
        assert!(matches!(
            error,
            CaptureError::UnknownLedgerKeys { count: 1 }
        ));
    }

    #[test]
    fn coherent_materialization_batches_at_200_and_retries_the_whole_attempt() {
        let fake = FakeTransport::m1();
        fake.mix_second_large_batch_once.set(true);
        let root = parse_contract_address(POOL_ID).unwrap();
        let keys: Vec<LedgerKey> = (1_000..1_401)
            .map(|index| data_key(root.clone(), index))
            .collect();

        let captured = builder(&fake)
            .capture(|env, _| {
                for key in &keys {
                    assert!(env
                        .host()
                        .get_ledger_entry(&Rc::new(key.clone()))
                        .unwrap()
                        .is_none());
                }
            })
            .unwrap();

        let calls = fake.batch_calls.borrow();
        assert!(calls.iter().all(|batch| batch.len() <= MAX_KEYS_PER_BATCH));
        let large: Vec<_> = calls
            .iter()
            .filter(|batch| batch.len() == MAX_KEYS_PER_BATCH)
            .collect();
        assert!(large.len() >= 4);
        assert!(fake.mixed_once.get());
        let first_batch_key = &large[0][0];
        assert!(
            large
                .iter()
                .filter(|batch| batch.contains(first_batch_key))
                .count()
                >= 2
        );
        assert!(captured.report().absent_entries() >= 401);
    }

    #[test]
    fn terminal_panic_returns_opaque_error_and_transport_failure_is_not_absence() {
        let fake = FakeTransport::m1();
        let panic_error = builder(&fake)
            .capture(|_, _| panic!("scenario-payload-must-not-leak"))
            .unwrap_err();
        let rendered = format!("{panic_error:?} {panic_error}");
        assert!(matches!(panic_error, CaptureError::ScenarioPanicked));
        assert!(!rendered.contains("scenario-payload-must-not-leak"));

        let failed_key = data_key(parse_contract_address(POOL_ID).unwrap(), 888);
        fake.fail_keys
            .borrow_mut()
            .insert(key_id(&failed_key).unwrap());
        let transport_error = builder(&fake)
            .capture(|env, _| {
                let _ = env.host().get_ledger_entry(&Rc::new(failed_key.clone()));
            })
            .unwrap_err();
        assert!(matches!(transport_error, CaptureError::Transport { .. }));
    }

    #[test]
    fn rpc_origin_discards_userinfo_path_query_and_fragment() {
        let origin = redact_rpc_origin(
            "https://user:secret@rpc.example.test:8443/private?token=secret#fragment",
        )
        .unwrap();
        assert_eq!(origin, "https://rpc.example.test:8443");
    }

    #[test]
    fn rpc_wire_preserves_v1_extension_and_rejects_missing_or_malformed_extension() {
        let snapshot =
            LedgerSnapshot::read_file("fixtures/mainnet/aquarius-xlm-usdc-cp/ledger.json").unwrap();
        let account_entry = snapshot
            .ledger_entries
            .iter()
            .map(|(_, (entry, _))| entry.as_ref())
            .find(|entry| matches!(entry.data, LedgerEntryData::Account(_)))
            .unwrap();
        let LedgerEntryData::Account(account) = &account_entry.data else {
            unreachable!();
        };
        let extension = LedgerEntryExt::V1(LedgerEntryExtensionV1 {
            sponsoring_id: SponsorshipDescriptor(Some(account.account_id.clone())),
            ext: LedgerEntryExtensionV1Ext::V0,
        });
        let wire = EntryWire {
            key: BASE64.encode(account_entry.to_key().to_xdr(Limits::none()).unwrap()),
            xdr: BASE64.encode(account_entry.data.to_xdr(Limits::none()).unwrap()),
            last_modified_ledger_seq: account_entry.last_modified_ledger_seq,
            live_until_ledger_seq: None,
            ext_xdr: Some(BASE64.encode(extension.to_xdr(Limits::none()).unwrap())),
        };

        let decoded = decode_wire_entry(wire, "getLedgerEntries").unwrap();
        assert_eq!(decoded.entry.ext, extension);

        let missing = EntryWire {
            key: BASE64.encode(account_entry.to_key().to_xdr(Limits::none()).unwrap()),
            xdr: BASE64.encode(account_entry.data.to_xdr(Limits::none()).unwrap()),
            last_modified_ledger_seq: account_entry.last_modified_ledger_seq,
            live_until_ledger_seq: None,
            ext_xdr: None,
        };
        assert!(decode_wire_entry(missing, "getLedgerEntries").is_err());

        let malformed = EntryWire {
            key: BASE64.encode(account_entry.to_key().to_xdr(Limits::none()).unwrap()),
            xdr: BASE64.encode(account_entry.data.to_xdr(Limits::none()).unwrap()),
            last_modified_ledger_seq: account_entry.last_modified_ledger_seq,
            live_until_ledger_seq: None,
            ext_xdr: Some("not-base64".to_string()),
        };
        assert!(decode_wire_entry(malformed, "getLedgerEntries").is_err());
    }

    #[test]
    fn ledger_metadata_validation_covers_live_archived_and_classic_entries() {
        let sequence = 100;
        let max_entry_ttl = 20;
        let root = parse_contract_address(POOL_ID).unwrap();
        let mut persistent = contract_data_entry(root, 1, 1);
        persistent.last_modified_ledger_seq = sequence;

        assert!(validate_entry_metadata(
            &persistent,
            Some(0),
            sequence,
            max_entry_ttl
        ));
        assert!(validate_entry_metadata(
            &persistent,
            Some(sequence),
            sequence,
            max_entry_ttl
        ));
        assert!(validate_entry_metadata(
            &persistent,
            Some(sequence + max_entry_ttl - 1),
            sequence,
            max_entry_ttl
        ));
        assert!(!validate_entry_metadata(
            &persistent,
            None,
            sequence,
            max_entry_ttl
        ));
        assert!(!validate_entry_metadata(
            &persistent,
            Some(sequence - 1),
            sequence,
            max_entry_ttl
        ));
        assert!(!validate_entry_metadata(
            &persistent,
            Some(sequence + max_entry_ttl),
            sequence,
            max_entry_ttl
        ));

        let mut temporary = persistent.clone();
        let LedgerEntryData::ContractData(data) = &mut temporary.data else {
            unreachable!();
        };
        data.durability = ContractDataDurability::Temporary;
        assert!(!validate_entry_metadata(
            &temporary,
            Some(0),
            sequence,
            max_entry_ttl
        ));
        assert!(validate_entry_metadata(
            &temporary,
            Some(sequence),
            sequence,
            max_entry_ttl
        ));

        let snapshot =
            LedgerSnapshot::read_file("fixtures/mainnet/aquarius-xlm-usdc-cp/ledger.json").unwrap();
        let account = snapshot
            .ledger_entries
            .iter()
            .map(|(_, (entry, _))| entry.as_ref())
            .find(|entry| matches!(entry.data, LedgerEntryData::Account(_)))
            .unwrap();
        assert!(validate_entry_metadata(
            account,
            None,
            snapshot.sequence_number,
            snapshot.max_entry_ttl
        ));
        assert!(!validate_entry_metadata(
            account,
            Some(0),
            snapshot.sequence_number,
            snapshot.max_entry_ttl
        ));

        persistent.last_modified_ledger_seq = sequence + 1;
        assert!(!validate_entry_metadata(
            &persistent,
            Some(sequence),
            sequence,
            max_entry_ttl
        ));
    }

    fn aquarius_scenario(env: &Env, root: &Address) {
        let pool = pool::Client::new(env, root);
        let tokens = pool.get_tokens();
        let usdc = tokens.get(1).unwrap();
        assert_eq!(usdc, Address::from_str(env, USDC_ID));
        let quote_before = pool.estimate_swap(&1, &0, &ONE_USDC);
        let reserves = pool.get_reserves();
        let amount = reserves.get(1).unwrap() / 10;
        let user =
            Address::try_from_val(env, &ScAddress::Contract(Hash([0x4b; 32]).into())).unwrap();

        env.mock_all_auths();
        StellarAssetClient::new(env, &usdc).mint(&user, &i128::try_from(amount).unwrap());
        env.set_auths(&[]);
        let transfer = MockAuthInvoke {
            contract: &usdc,
            fn_name: "transfer",
            args: (user.clone(), root.clone(), i128::try_from(amount).unwrap()).into_val(env),
            sub_invokes: &[],
        };
        let children = [transfer];
        let swap = MockAuthInvoke {
            contract: root,
            fn_name: "swap",
            args: (user.clone(), 1_u32, 0_u32, amount, 0_u128).into_val(env),
            sub_invokes: &children,
        };
        pool.mock_auths(&[MockAuth {
            address: &user,
            invoke: &swap,
        }])
        .swap(&user, &1, &0, &amount, &0);
        assert!(pool.estimate_swap(&1, &0, &ONE_USDC) < quote_before);
    }

    fn test_bundle_path(label: &str) -> PathBuf {
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "kanatoko-{label}-{}-{sequence}.capture.json",
            std::process::id()
        ))
    }

    fn refresh_bundle_digests(bundle: &mut CaptureBundleV1) {
        let fixture = FrozenFixture::from_snapshot(
            bundle.ledger_snapshot.clone(),
            &bundle.provenance.network_passphrase,
        )
        .unwrap();
        let coverage = bundle_coverage(&bundle.ledger_snapshot, &bundle.known_absent).unwrap();
        let inventory = inventory_digest(&coverage).unwrap();
        bundle.ledger_digest = hex(fixture.ledger_digest());
        bundle.inventory_digest = hex(inventory);
        bundle.canonical_digest = hex(canonical_bundle_digest(
            &bundle.root_contract,
            &bundle.known_absent,
            &bundle.report,
            &bundle.provenance,
            &fixture.ledger_digest(),
            &inventory,
        )
        .unwrap());
    }

    fn builder(fake: &Rc<FakeTransport>) -> CaptureBuilder {
        CaptureBuilder::with_transport(fake.clone(), MAINNET_PASSPHRASE, POOL_ID)
    }

    fn data_key(contract: ScAddress, value: u32) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract,
            key: ScVal::U32(value),
            durability: ContractDataDurability::Persistent,
        })
    }

    fn contract_data_entry(contract: ScAddress, key: u32, value: u32) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 0,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                contract,
                key: ScVal::U32(key),
                val: ScVal::U32(value),
                durability: ContractDataDurability::Persistent,
                ext: ExtensionPoint::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    struct FakeTransport {
        network: Network,
        anchor: Anchor,
        entries: BTreeMap<KeyId, FetchedEntry>,
        batch_calls: RefCell<Vec<Vec<KeyId>>>,
        reads: Cell<u64>,
        fail_keys: RefCell<BTreeSet<KeyId>>,
        mix_second_large_batch_once: Cell<bool>,
        large_batches_seen: Cell<usize>,
        mixed_once: Cell<bool>,
    }

    impl FakeTransport {
        fn m1() -> Rc<Self> {
            let snapshot =
                LedgerSnapshot::read_file("fixtures/mainnet/aquarius-xlm-usdc-cp/ledger.json")
                    .unwrap();
            let mut entries = BTreeMap::new();
            for (_, (entry, live_until)) in &snapshot.ledger_entries {
                entries.insert(
                    key_id(&entry.to_key()).unwrap(),
                    FetchedEntry {
                        entry: Rc::new((**entry).clone()),
                        live_until: *live_until,
                    },
                );
            }
            let settings = StateArchivalSettings {
                max_entry_ttl: snapshot.max_entry_ttl,
                min_temporary_ttl: snapshot.min_temp_entry_ttl,
                min_persistent_ttl: snapshot.min_persistent_entry_ttl,
                persistent_rent_rate_denominator: 1,
                temp_rent_rate_denominator: 1,
                max_entries_to_archive: 1,
                live_soroban_state_size_window_sample_size: 1,
                live_soroban_state_size_window_sample_period: 1,
                eviction_scan_size: 1,
                starting_eviction_scan_level: 1,
            };
            let config = LedgerEntry {
                last_modified_ledger_seq: snapshot.sequence_number,
                data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(settings)),
                ext: LedgerEntryExt::V0,
            };
            entries.insert(
                key_id(&config.to_key()).unwrap(),
                FetchedEntry {
                    entry: Rc::new(config),
                    live_until: None,
                },
            );
            Rc::new(Self {
                network: Network {
                    passphrase: MAINNET_PASSPHRASE.to_string(),
                    protocol_version: snapshot.protocol_version,
                },
                anchor: Anchor {
                    sequence: snapshot.sequence_number,
                    hash: "fake-coherent-ledger".to_string(),
                    protocol_version: snapshot.protocol_version,
                    timestamp: snapshot.timestamp,
                    base_reserve: snapshot.base_reserve,
                },
                entries,
                batch_calls: RefCell::new(Vec::new()),
                reads: Cell::new(0),
                fail_keys: RefCell::new(BTreeSet::new()),
                mix_second_large_batch_once: Cell::new(false),
                large_batches_seen: Cell::new(0),
                mixed_once: Cell::new(false),
            })
        }

        fn ledger_entry_reads(&self) -> u64 {
            self.reads.get()
        }
    }

    impl Transport for FakeTransport {
        fn network(&self) -> Result<Network, TransportFailure> {
            Ok(self.network.clone())
        }

        fn latest_ledger(&self) -> Result<Anchor, TransportFailure> {
            Ok(self.anchor.clone())
        }

        fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<EntryBatch, TransportFailure> {
            self.reads.set(self.reads.get() + 1);
            let ids: Vec<KeyId> = keys.iter().map(|key| key_id(key).unwrap()).collect();
            self.batch_calls.borrow_mut().push(ids.clone());
            if ids.iter().any(|id| self.fail_keys.borrow().contains(id)) {
                return Err(TransportFailure {
                    operation: "getLedgerEntries",
                });
            }
            let mut latest_ledger = self.anchor.sequence;
            if keys.len() == MAX_KEYS_PER_BATCH && self.mix_second_large_batch_once.get() {
                let seen = self.large_batches_seen.get() + 1;
                self.large_batches_seen.set(seen);
                if seen == 2 && !self.mixed_once.replace(true) {
                    latest_ledger += 1;
                }
            }
            Ok(EntryBatch {
                latest_ledger,
                entries: ids
                    .iter()
                    .filter_map(|id| self.entries.get(id).cloned())
                    .collect(),
            })
        }
    }
}
