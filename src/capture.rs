//! Execution-driven capture of the ledger state a Soroban scenario actually
//! uses.

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
    thread,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soroban_env_host::{
    testutils::call_with_suppressed_panic_hook,
    xdr::{
        ConfigSettingEntry, ConfigSettingId, ContractDataDurability, ContractExecutable, Hash,
        LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerHeader, LedgerKey,
        LedgerKeyConfigSetting, LedgerKeyContractCode, LedgerKeyContractData, Limits, ReadXdr,
        ScAddress, ScErrorCode, ScErrorType, ScVal, StateArchivalSettings, WriteXdr,
    },
};
use soroban_ledger_snapshot::LedgerSnapshot;
use soroban_sdk::{
    testutils::{HostError, LedgerInfo, SnapshotSource, SnapshotSourceInput},
    Address, Env,
};
use thiserror::Error;

use crate::{
    runtime::configure_fork_env, FixtureError, FrozenFixture, StrictFork,
    SUPPORTED_PROTOCOL_VERSION,
};

pub(crate) const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
pub(crate) const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";
const MAX_COHERENCE_ATTEMPTS: usize = 8;
const MAX_DISCOVERY_ROUNDS: usize = 16;
const MAX_KEYS_PER_BATCH: usize = 200;
const RPC_RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];
const RPC_MAX_RETRY_AFTER: Duration = Duration::from_secs(5);
const CAPTURE_BUNDLE_SCHEMA_VERSION: u32 = 2;
const INVENTORY_DIGEST_DOMAIN_V1: &[u8] = b"KANATOKO\0CAPTURE-INVENTORY\0V1\0";
const BUNDLE_DIGEST_DOMAIN_V2: &[u8] = b"KANATOKO\0CAPTURE-BUNDLE\0V2\0";
const LEGACY_BUNDLE_DIGEST_DOMAIN_V1: &[u8] = b"KANATOKO\0CAPTURE-BUNDLE\0V1\0";
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) type KeyId = Vec<u8>;
type PresentEntry = (Rc<LedgerEntry>, Option<u32>);

/// Ledger keys explicitly owned by local test setup rather than the captured
/// network. Each scenario pass gets a fresh registry shared by its facade and
/// snapshot source.
#[derive(Default)]
pub(crate) struct LocalLedger {
    addresses: RefCell<BTreeSet<ScAddress>>,
    code_hashes: RefCell<BTreeSet<Hash>>,
}

impl LocalLedger {
    pub(crate) fn mark_address(&self, address: ScAddress) {
        self.addresses.borrow_mut().insert(address);
    }

    pub(crate) fn mark_code(&self, hash: [u8; 32]) {
        self.code_hashes.borrow_mut().insert(Hash(hash));
    }

    fn owns(&self, key: &LedgerKey) -> bool {
        match key {
            LedgerKey::ContractCode(code) => self.code_hashes.borrow().contains(&code.hash),
            LedgerKey::ContractData(data) => self.addresses.borrow().contains(&data.contract),
            LedgerKey::Account(account) => self
                .addresses
                .borrow()
                .contains(&ScAddress::Account(account.account_id.clone())),
            LedgerKey::Trustline(trustline) => self
                .addresses
                .borrow()
                .contains(&ScAddress::Account(trustline.account_id.clone())),
            _ => false,
        }
    }
}

/// Builds an execution-driven, RPC-backed capture.
///
/// The URL is held only by the HTTP transport. Debug output and capture
/// provenance retain only the scheme and hostname/port. Userinfo, path, query,
/// and fragment are discarded. A credential embedded in the hostname cannot
/// be recognized and will remain visible, so credentials must not be placed
/// there.
pub struct CaptureBuilder {
    network_passphrase: String,
    rpc_origin: String,
    transport: Rc<dyn Transport>,
    rpc_rate_limiter: Option<Rc<RequestRateLimiter>>,
    max_coherence_attempts: usize,
    max_discovery_rounds: usize,
}

impl CaptureBuilder {
    /// Creates a capture builder for Stellar public mainnet.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL has no HTTP(S) origin.
    pub fn mainnet(rpc_url: impl Into<String>) -> Result<Self, CaptureError> {
        Self::rpc(rpc_url, MAINNET_PASSPHRASE)
    }

    /// Creates a capture builder for Stellar public testnet.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL has no HTTP(S) origin.
    pub fn testnet(rpc_url: impl Into<String>) -> Result<Self, CaptureError> {
        Self::rpc(rpc_url, TESTNET_PASSPHRASE)
    }

    /// Creates a capture builder for an explicit RPC network and passphrase.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL has no HTTP(S) origin.
    pub fn rpc(
        rpc_url: impl Into<String>,
        network_passphrase: impl Into<String>,
    ) -> Result<Self, CaptureError> {
        let rpc_url = rpc_url.into();
        let rpc_origin = redact_rpc_origin(&rpc_url)?;
        let rpc_rate_limiter = Rc::new(RequestRateLimiter::new(0));
        let transport = Rc::new(HttpTransport::new(rpc_url, rpc_rate_limiter.clone()));
        Ok(Self {
            network_passphrase: network_passphrase.into(),
            rpc_origin,
            transport,
            rpc_rate_limiter: Some(rpc_rate_limiter),
            max_coherence_attempts: MAX_COHERENCE_ATTEMPTS,
            max_discovery_rounds: MAX_DISCOVERY_ROUNDS,
        })
    }

    /// Limits read-only RPC requests made by this builder, including retry
    /// attempts, to at most the supplied requests per second. The default and
    /// `0` both mean unlimited. Separate builders do not share a quota.
    #[must_use]
    pub fn rpc_rate_limit(self, max_requests_per_second: u32) -> Self {
        if let Some(limiter) = &self.rpc_rate_limiter {
            limiter.set_requests_per_second(max_requests_per_second);
        }
        self
    }

    /// Captures the fixed point of ledger keys touched by `scenario`.
    ///
    /// A fresh [`Env`] is supplied on every run, so generated clients and
    /// addresses must be recreated inside the closure. The closure can run
    /// several times and must be deterministic, repeatable, and free of
    /// external side effects. Scenario panics are converted to a terminal error
    /// only after key discovery stops. Panic output is suppressed during these
    /// discovery passes.
    ///
    /// # Errors
    ///
    /// Fails closed on network/protocol mismatch, transport or XDR failure,
    /// incoherent ledger batches, missing referenced code, a terminal scenario
    /// panic, or a bounded fixed-point failure.
    pub fn capture<F>(&self, scenario: F) -> Result<CapturedFixture, CaptureError>
    where
        F: Fn(&Env),
    {
        self.capture_ref(&|env, _, _| scenario(env))
    }

    pub(crate) fn capture_with_local<F>(&self, scenario: F) -> Result<CapturedFixture, CaptureError>
    where
        F: Fn(&Env, Rc<LocalLedger>, Rc<TrackingSource>),
    {
        self.capture_ref(&scenario)
    }

    fn capture_ref<F>(&self, scenario: &F) -> Result<CapturedFixture, CaptureError>
    where
        F: Fn(&Env, Rc<LocalLedger>, Rc<TrackingSource>),
    {
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
        let mut materialized = self.materialize(&keys)?;
        materialized = self.expand_code_closure(&mut keys, materialized)?;

        for round in 1..=self.max_discovery_rounds {
            let local = Rc::new(LocalLedger::default());
            let source = Rc::new(TrackingSource::rpc_with_local(
                materialized.coverage.clone(),
                self.transport.clone(),
                local.clone(),
            ));
            let env = env_from_materialized(&materialized, source.clone());
            let outcome = call_with_suppressed_panic_hook(AssertUnwindSafe(|| {
                scenario(&env, local.clone(), source.clone());
            }));

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
                if local.owns(&key) {
                    continue;
                }
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
    fn with_transport(transport: Rc<dyn Transport>, network_passphrase: impl Into<String>) -> Self {
        Self {
            network_passphrase: network_passphrase.into(),
            rpc_origin: "https://fake.invalid".to_string(),
            transport,
            rpc_rate_limiter: None,
            max_coherence_attempts: MAX_COHERENCE_ATTEMPTS,
            max_discovery_rounds: MAX_DISCOVERY_ROUNDS,
        }
    }
}

impl fmt::Debug for CaptureBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureBuilder")
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
    /// then renamed over `path`. The separate frozen ledger snapshot format is
    /// not changed by this API.
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
    /// Present/Absent coverage, or missing referenced WASM.
    pub fn from_file(
        path: impl AsRef<Path>,
        expected_network_passphrase: &str,
    ) -> Result<Self, CaptureError> {
        let bytes = fs::read(path.as_ref()).map_err(|source| CaptureError::CaptureBundleIo {
            operation: "read",
            source,
        })?;
        Self::from_bundle_bytes(&bytes, expected_network_passphrase)
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
        F: FnOnce(&Env) -> R,
    {
        self.replay_with_local(|env, _, _| scenario(env))
    }

    pub(crate) fn replay_with_local<F, R>(&self, scenario: F) -> Result<R, CaptureError>
    where
        F: FnOnce(&Env, Rc<LocalLedger>, Rc<TrackingSource>) -> R,
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
        let local = Rc::new(LocalLedger::default());
        let source = Rc::new(TrackingSource::strict_with_local(
            self.coverage.clone(),
            local.clone(),
        ));
        let env = env_from_materialized(&materialized, source.clone());
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            scenario(&env, local.clone(), source.clone())
        }));

        let mut unknown = source.unknown_keys();
        for (key, _) in env
            .host()
            .get_stored_entries()
            .map_err(|_| CaptureError::HostInspection)?
        {
            if local.owns(&key) {
                continue;
            }
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

    /// Creates an offline mutable fork that retains strict Present/Absent
    /// coverage. Keys outside that inventory fail closed after every preview,
    /// applied invocation, mutation, and revert.
    #[must_use]
    pub fn fork(&self) -> StrictFork {
        StrictFork::from_captured(self.fixture.ledger_snapshot(), self.coverage.clone())
    }

    fn to_bundle(&self) -> Result<CaptureBundleV2, CaptureError> {
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
            &known_absent,
            &report,
            &provenance,
            &ledger_digest,
            &inventory_digest,
        )?;
        Ok(CaptureBundleV2 {
            schema_version: CAPTURE_BUNDLE_SCHEMA_VERSION,
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
        bundle: CaptureBundleV2,
        expected_network_passphrase: &str,
    ) -> Result<Self, CaptureError> {
        let digests = validate_bundle_envelope(&bundle, expected_network_passphrase)?;
        let fixture =
            FrozenFixture::from_snapshot(bundle.ledger_snapshot, expected_network_passphrase)?;
        if fixture.ledger_digest() != digests.ledger {
            return Err(CaptureError::CaptureBundleIntegrity);
        }

        let coverage = bundle_coverage(fixture.ledger_snapshot(), &bundle.known_absent)?;
        let (present_entries, absent_entries) = validate_bundle_coverage(
            &coverage,
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
            coverage,
            report,
            provenance,
        })
    }

    fn from_bundle_bytes(
        bytes: &[u8],
        expected_network_passphrase: &str,
    ) -> Result<Self, CaptureError> {
        let version: BundleVersion =
            serde_json::from_slice(bytes).map_err(|_| CaptureError::MalformedCaptureBundle)?;
        match version.schema_version {
            CAPTURE_BUNDLE_SCHEMA_VERSION => {
                let bundle: CaptureBundleV2 = serde_json::from_slice(bytes)
                    .map_err(|_| CaptureError::MalformedCaptureBundle)?;
                Self::from_bundle(bundle, expected_network_passphrase)
            }
            1 => {
                let bundle: LegacyCaptureBundleV1 = serde_json::from_slice(bytes)
                    .map_err(|_| CaptureError::MalformedCaptureBundle)?;
                Self::from_legacy_bundle(bundle, expected_network_passphrase)
            }
            found => Err(CaptureError::UnsupportedCaptureBundleSchema {
                found,
                supported: CAPTURE_BUNDLE_SCHEMA_VERSION,
            }),
        }
    }

    fn from_legacy_bundle(
        bundle: LegacyCaptureBundleV1,
        expected_network_passphrase: &str,
    ) -> Result<Self, CaptureError> {
        let digests = validate_legacy_bundle_envelope(&bundle, expected_network_passphrase)?;
        let fixture =
            FrozenFixture::from_snapshot(bundle.ledger_snapshot, expected_network_passphrase)?;
        if fixture.ledger_digest() != digests.ledger {
            return Err(CaptureError::CaptureBundleIntegrity);
        }

        let root = parse_legacy_contract_address(&bundle.root_contract)?;
        let coverage = bundle_coverage(fixture.ledger_snapshot(), &bundle.known_absent)?;
        ensure_legacy_root_present(&root, &coverage)
            .map_err(|_| CaptureError::CaptureBundleIntegrity)?;
        let (present_entries, absent_entries) = validate_bundle_coverage(
            &coverage,
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
            coverage,
            report,
            provenance,
        })
    }
}

#[derive(Deserialize)]
struct BundleVersion {
    schema_version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureBundleV2 {
    schema_version: u32,
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
struct LegacyCaptureBundleV1 {
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
    bundle: &CaptureBundleV2,
    expected_network_passphrase: &str,
) -> Result<BundleDigests, CaptureError> {
    if bundle.schema_version != CAPTURE_BUNDLE_SCHEMA_VERSION {
        return Err(CaptureError::UnsupportedCaptureBundleSchema {
            found: bundle.schema_version,
            supported: CAPTURE_BUNDLE_SCHEMA_VERSION,
        });
    }
    validate_bundle_metadata(
        &bundle.ledger_snapshot,
        &bundle.report,
        &bundle.provenance,
        expected_network_passphrase,
    )?;

    let ledger = decode_hex_digest(&bundle.ledger_digest)?;
    let inventory = decode_hex_digest(&bundle.inventory_digest)?;
    let actual_canonical = decode_hex_digest(&bundle.canonical_digest)?;
    let expected_canonical = canonical_bundle_digest(
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

fn validate_bundle_metadata(
    ledger_snapshot: &LedgerSnapshot,
    report: &BundleReport,
    provenance: &BundleProvenance,
    expected_network_passphrase: &str,
) -> Result<(), CaptureError> {
    if provenance.network_passphrase != expected_network_passphrase {
        return Err(CaptureError::NetworkMismatch);
    }
    if redact_rpc_origin(&provenance.rpc_origin)? != provenance.rpc_origin {
        return Err(CaptureError::MalformedCaptureBundle);
    }
    let discovery_rounds = usize::try_from(report.discovery_rounds)
        .map_err(|_| CaptureError::MalformedCaptureBundle)?;
    if provenance.ledger_sequence != ledger_snapshot.sequence_number
        || provenance.protocol_version != ledger_snapshot.protocol_version
        || provenance.ledger_hash.is_empty()
        || discovery_rounds == 0
        || discovery_rounds > MAX_DISCOVERY_ROUNDS
        || report.final_replay_rpc_reads != 0
    {
        return Err(CaptureError::CaptureBundleIntegrity);
    }
    Ok(())
}

fn validate_legacy_bundle_envelope(
    bundle: &LegacyCaptureBundleV1,
    expected_network_passphrase: &str,
) -> Result<BundleDigests, CaptureError> {
    if bundle.schema_version != 1 {
        return Err(CaptureError::UnsupportedCaptureBundleSchema {
            found: bundle.schema_version,
            supported: CAPTURE_BUNDLE_SCHEMA_VERSION,
        });
    }
    validate_bundle_metadata(
        &bundle.ledger_snapshot,
        &bundle.report,
        &bundle.provenance,
        expected_network_passphrase,
    )?;

    let ledger = decode_hex_digest(&bundle.ledger_digest)?;
    let inventory = decode_hex_digest(&bundle.inventory_digest)?;
    let actual_canonical = decode_hex_digest(&bundle.canonical_digest)?;
    let expected_canonical = legacy_canonical_bundle_digest(
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
    report: &BundleReport,
    expected_inventory_digest: &[u8; 32],
    ledger_sequence: u32,
    max_entry_ttl: u32,
) -> Result<(usize, usize), CaptureError> {
    if validate_coverage(coverage, ledger_sequence, max_entry_ttl).is_err()
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
pub(crate) enum LookupState {
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

pub(crate) struct TrackingSource {
    cache: RefCell<BTreeMap<KeyId, LookupState>>,
    requested: RefCell<BTreeMap<KeyId, LedgerKey>>,
    unknown: RefCell<BTreeSet<KeyId>>,
    transport: Option<Rc<dyn Transport>>,
    local: Rc<LocalLedger>,
    rpc_reads: Cell<u64>,
    failure: Cell<Option<&'static str>>,
}

impl TrackingSource {
    fn rpc_with_local(
        cache: BTreeMap<KeyId, LookupState>,
        transport: Rc<dyn Transport>,
        local: Rc<LocalLedger>,
    ) -> Self {
        Self::new(cache, Some(transport), local)
    }

    fn strict_with_local(cache: BTreeMap<KeyId, LookupState>, local: Rc<LocalLedger>) -> Self {
        Self::new(cache, None, local)
    }

    fn new(
        cache: BTreeMap<KeyId, LookupState>,
        transport: Option<Rc<dyn Transport>>,
        local: Rc<LocalLedger>,
    ) -> Self {
        Self {
            cache: RefCell::new(cache),
            requested: RefCell::new(BTreeMap::new()),
            unknown: RefCell::new(BTreeSet::new()),
            transport,
            local,
            rpc_reads: Cell::new(0),
            failure: Cell::new(None),
        }
    }

    fn requested_keys(&self) -> BTreeMap<KeyId, LedgerKey> {
        self.requested.borrow().clone()
    }

    pub(crate) fn unknown_keys(&self) -> BTreeSet<KeyId> {
        self.unknown.borrow().clone()
    }

    pub(crate) fn rpc_reads(&self) -> u64 {
        self.rpc_reads.get()
    }

    pub(crate) fn observe(&self, key: &Rc<LedgerKey>) -> Result<(), HostError> {
        self.get(key).map(|_| ())
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
        if self.failure.get().is_some() {
            return Err(Self::host_error());
        }
        let Ok(id) = key_id(key) else {
            self.failure.set(Some("ledger-key-encode"));
            return Err(Self::host_error());
        };
        if self.local.owns(key) {
            return Ok(None);
        }
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

struct RequestRateLimiter {
    min_interval: Cell<Duration>,
    next_request: Cell<Option<Instant>>,
}

impl RequestRateLimiter {
    fn new(max_requests_per_second: u32) -> Self {
        Self {
            min_interval: Cell::new(request_interval(max_requests_per_second)),
            next_request: Cell::new(None),
        }
    }

    fn set_requests_per_second(&self, max_requests_per_second: u32) {
        self.min_interval
            .set(request_interval(max_requests_per_second));
        self.next_request.set(None);
    }

    fn reserve_delay(&self, now: Instant) -> Duration {
        let interval = self.min_interval.get();
        if interval.is_zero() {
            self.next_request.set(None);
            return Duration::ZERO;
        }
        let Some(next_request) = self.next_request.get() else {
            self.next_request.set(Some(now + interval));
            return Duration::ZERO;
        };
        if next_request <= now {
            self.next_request.set(Some(now + interval));
            return Duration::ZERO;
        }
        self.next_request.set(Some(next_request + interval));
        next_request.duration_since(now)
    }

    fn wait(&self) {
        let delay = self.reserve_delay(Instant::now());
        if !delay.is_zero() {
            thread::sleep(delay);
        }
    }
}

fn request_interval(max_requests_per_second: u32) -> Duration {
    if max_requests_per_second == 0 {
        return Duration::ZERO;
    }
    let nanos = 1_000_000_000_u64.div_ceil(u64::from(max_requests_per_second));
    Duration::from_nanos(nanos)
}

struct HttpTransport {
    rpc_url: String,
    agent: ureq::Agent,
    rate_limiter: Rc<RequestRateLimiter>,
}

impl HttpTransport {
    fn new(rpc_url: String, rate_limiter: Rc<RequestRateLimiter>) -> Self {
        Self {
            rpc_url,
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(30))
                .build(),
            rate_limiter,
        }
    }

    fn rpc<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        params: &serde_json::Value,
    ) -> Result<T, TransportFailure> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": operation,
            "params": params,
        });
        let response = send_read_rpc_with_retry(
            || {
                self.rate_limiter.wait();
                self.agent.post(&self.rpc_url).send_json(request.clone())
            },
            thread::sleep,
        )
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

// All RPC methods routed through this helper are read-only. Keep transaction
// submission out of this retry path.
fn send_read_rpc_with_retry(
    mut send: impl FnMut() -> Result<ureq::Response, ureq::Error>,
    mut sleep: impl FnMut(Duration),
) -> Result<ureq::Response, Box<ureq::Error>> {
    let mut retry = 0_usize;
    loop {
        match send() {
            Err(ureq::Error::Status(status, response))
                if is_retryable_http_status(status) && retry < RPC_RETRY_BACKOFF.len() =>
            {
                let Some(delay) =
                    retry_delay(RPC_RETRY_BACKOFF[retry], response.header("Retry-After"))
                else {
                    return Err(Box::new(ureq::Error::Status(status, response)));
                };
                retry += 1;
                sleep(delay);
            }
            result => return result.map_err(Box::new),
        }
    }
}

const fn is_retryable_http_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn retry_delay(backoff: Duration, retry_after: Option<&str>) -> Option<Duration> {
    let Some(retry_after) = retry_after else {
        return Some(backoff);
    };
    let Ok(seconds) = retry_after.trim().parse::<u64>() else {
        return Some(backoff);
    };
    let retry_after = Duration::from_secs(seconds);
    // Do not violate a long server-requested pause by retrying early.
    if retry_after > RPC_MAX_RETRY_AFTER {
        return None;
    }
    Some(backoff.max(retry_after))
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

fn ensure_legacy_root_present(
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
            _ => Err(CaptureError::CaptureBundleIntegrity),
        },
        _ => Err(CaptureError::CaptureBundleIntegrity),
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
    configure_fork_env(&mut env);
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
    known_absent: &[String],
    report: &BundleReport,
    provenance: &BundleProvenance,
    ledger_digest: &[u8; 32],
    inventory_digest: &[u8; 32],
) -> Result<[u8; 32], CaptureError> {
    let mut digest = Sha256::new();
    digest.update(BUNDLE_DIGEST_DOMAIN_V2);
    digest.update(CAPTURE_BUNDLE_SCHEMA_VERSION.to_be_bytes());
    update_bundle_digest(
        &mut digest,
        known_absent,
        report,
        provenance,
        ledger_digest,
        inventory_digest,
    )?;
    Ok(digest.finalize().into())
}

fn legacy_canonical_bundle_digest(
    root_contract: &str,
    known_absent: &[String],
    report: &BundleReport,
    provenance: &BundleProvenance,
    ledger_digest: &[u8; 32],
    inventory_digest: &[u8; 32],
) -> Result<[u8; 32], CaptureError> {
    let mut digest = Sha256::new();
    digest.update(LEGACY_BUNDLE_DIGEST_DOMAIN_V1);
    digest.update(1_u32.to_be_bytes());
    digest_field(&mut digest, root_contract.as_bytes());
    update_bundle_digest(
        &mut digest,
        known_absent,
        report,
        provenance,
        ledger_digest,
        inventory_digest,
    )?;
    Ok(digest.finalize().into())
}

fn update_bundle_digest(
    digest: &mut Sha256,
    known_absent: &[String],
    report: &BundleReport,
    provenance: &BundleProvenance,
    ledger_digest: &[u8; 32],
    inventory_digest: &[u8; 32],
) -> Result<(), CaptureError> {
    digest.update((known_absent.len() as u64).to_be_bytes());
    for encoded in known_absent {
        let key = BASE64
            .decode(encoded)
            .map_err(|_| CaptureError::MalformedCaptureBundle)?;
        digest_field(digest, &key);
    }
    digest.update(report.discovery_rounds.to_be_bytes());
    digest.update(report.present_entries.to_be_bytes());
    digest.update(report.absent_entries.to_be_bytes());
    digest.update(report.final_replay_rpc_reads.to_be_bytes());
    digest_field(digest, provenance.rpc_origin.as_bytes());
    digest_field(digest, provenance.network_passphrase.as_bytes());
    digest.update(provenance.ledger_sequence.to_be_bytes());
    digest_field(digest, provenance.ledger_hash.as_bytes());
    digest.update(provenance.protocol_version.to_be_bytes());
    digest.update(ledger_digest);
    digest.update(inventory_digest);
    Ok(())
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

fn parse_legacy_contract_address(value: &str) -> Result<ScAddress, CaptureError> {
    let mut env = Env::default();
    configure_fork_env(&mut env);
    let address = catch_unwind(AssertUnwindSafe(|| Address::from_str(&env, value)))
        .map_err(|_| CaptureError::MalformedCaptureBundle)?;
    let address: ScAddress = (&address).into();
    if matches!(address, ScAddress::Contract(_)) {
        Ok(address)
    } else {
        Err(CaptureError::MalformedCaptureBundle)
    }
}

fn key_id(key: &LedgerKey) -> Result<KeyId, CaptureError> {
    key.to_xdr(Limits::none()).map_err(|_| CaptureError::Xdr)
}

pub(crate) fn contract_instance_key(contract: ScAddress) -> LedgerKey {
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

#[cfg(all(test, kanatoko_protocol_27_fixtures))]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::{BTreeMap, BTreeSet, VecDeque},
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
        #![allow(clippy::ref_option, clippy::too_many_arguments)]

        soroban_sdk::contractimport!(
            file = "fixtures/mainnet/aquarius-xlm-usdc-cp/pool.wasm",
            sha256 = "ae0da5a84b15805c5c7931ac567a8d1b34be3f26b483993d9ff80cb2c3de9852",
        );
    }

    mod local_wrapper {
        #![allow(clippy::too_many_arguments)]

        soroban_sdk::contractimport!(
            file = "fixtures/wasm/kanatoko_aquarius_wrapper.wasm",
            sha256 = "798c959e1e22093c49b4ec6636aafed14e889614fb243426abe5023b30c17520",
        );
    }

    mod local_stateful {
        soroban_sdk::contractimport!(
            file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
            sha256 = "6f6f469798b686cc485ad207f32e3f77009c4b69ab2437d9bdca97f149b54ba8",
        );
    }

    const POOL_ID: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
    const USDC_ID: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
    const ONE_USDC: u128 = 10_000_000;

    #[test]
    fn empty_scenario_does_not_seed_any_contract() {
        let fake = FakeTransport::from_aquarius_snapshot();
        let captured = builder(&fake).capture(|_| {}).unwrap();

        assert!(captured
            .frozen_fixture()
            .ledger_snapshot()
            .ledger_entries
            .is_empty());
        assert_eq!(captured.report().present_entries(), 0);
        assert_eq!(captured.report().absent_entries(), 0);
        assert_eq!(captured.report().final_replay_rpc_reads(), 0);
    }

    #[test]
    fn touched_contract_instance_and_matching_wasm_are_discovered() {
        let fake = FakeTransport::from_aquarius_snapshot();
        let captured = builder(&fake)
            .capture(|env| {
                let pool_id = Address::from_str(env, POOL_ID);
                assert_eq!(pool::Client::new(env, &pool_id).get_tokens().len(), 2);
            })
            .unwrap();
        let contract = parse_legacy_contract_address(POOL_ID).unwrap();
        let contract_id = key_id(&contract_instance_key(contract)).unwrap();

        let contract_entry = captured
            .frozen_fixture()
            .ledger_snapshot()
            .ledger_entries
            .iter()
            .find(|(key, _)| key_id(key).unwrap() == contract_id)
            .unwrap();
        let LedgerEntryData::ContractData(data) = &contract_entry.1 .0.data else {
            panic!("contract instance must be contract data");
        };
        let ScVal::ContractInstance(instance) = &data.val else {
            panic!("contract instance must contain a contract instance value");
        };
        let ContractExecutable::Wasm(hash) = &instance.executable else {
            panic!("Aquarius pool must be WASM");
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
    fn aquarius_snapshot_auto_discovers_quote_mint_swap_dependencies_and_replays_offline() {
        let fake = FakeTransport::from_aquarius_snapshot();
        let captured = builder(&fake).capture(aquarius_scenario).unwrap();
        assert!(captured.report().present_entries() >= 9);
        assert!(captured.report().absent_entries() >= 1);
        let reads_before = fake.ledger_entry_reads();
        captured.replay(aquarius_scenario).unwrap();
        assert_eq!(fake.ledger_entry_reads(), reads_before);
    }

    #[test]
    fn auto_runner_mixes_imported_abi_and_dynamic_invocations_then_reuses_cache_offline() {
        use crate::auto::{AutoRunner, CacheStatus};

        let fake = FakeTransport::from_aquarius_snapshot();
        let path = test_bundle_path("auto-runner");
        let first = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .run(mixed_auto_scenario)
            .unwrap();
        assert_eq!(first.cache_status(), CacheStatus::Created);
        assert_eq!(first.fixture().report().final_replay_rpc_reads(), 0);
        let reads_after_capture = fake.ledger_entry_reads();

        let second = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .offline()
            .run(mixed_auto_scenario)
            .unwrap();
        assert_eq!(second.cache_status(), CacheStatus::Hit);
        assert_eq!(fake.ledger_entry_reads(), reads_after_capture);
        assert_eq!(second.fixture().report().final_replay_rpc_reads(), 0);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn auto_runner_cache_hit_ignores_invalid_rpc_override() {
        use crate::auto::{mainnet, AutoRunner, CacheStatus};

        let fake = FakeTransport::from_aquarius_snapshot();
        let path = test_bundle_path("auto-runner-invalid-rpc-cache-hit");
        AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .run(|_| {})
            .unwrap();
        let reads_after_capture = fake.ledger_entry_reads();

        let replayed = mainnet()
            .rpc_url("this-is-not-a-url")
            .cache(&path)
            .offline()
            .run(|_| {})
            .unwrap();

        assert_eq!(replayed.cache_status(), CacheStatus::Hit);
        assert_eq!(fake.ledger_entry_reads(), reads_after_capture);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn auto_runner_deploys_local_wasm_that_calls_captured_contract_offline() {
        use crate::auto::{AutoRunner, CacheStatus};

        let fake = FakeTransport::from_aquarius_snapshot();
        let path = test_bundle_path("auto-runner-local-wasm");
        let scenario = |fork: &crate::auto::ScenarioFork<'_>| {
            let pool = fork.contract(POOL_ID);
            let candidate = fork.deploy(local_wrapper::WASM, (pool.clone(),));
            let direct = fork.invoke::<u128>(&pool, "estimate_swap", (1_u32, 0_u32, ONE_USDC));
            let through_candidate =
                local_wrapper::Client::new(fork.env(), &candidate).estimate_swap(&1, &0, &ONE_USDC);
            assert_eq!(through_candidate, direct);
        };

        let first = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .run(scenario)
            .unwrap();
        assert_eq!(first.cache_status(), CacheStatus::Created);
        let local_code_hash = Hash(Sha256::digest(local_wrapper::WASM).into());
        assert!(first
            .fixture()
            .frozen_fixture()
            .ledger_snapshot()
            .ledger_entries
            .iter()
            .all(|(_, (entry, _))| !matches!(
                &entry.data,
                LedgerEntryData::ContractCode(code) if code.hash == local_code_hash
            )));
        let reads_after_capture = fake.ledger_entry_reads();

        let second = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .offline()
            .run(scenario)
            .unwrap();
        assert_eq!(second.cache_status(), CacheStatus::Hit);
        assert_eq!(fake.ledger_entry_reads(), reads_after_capture);

        let changed_candidate = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .offline()
            .run(|fork| {
                let candidate = fork.deploy(local_stateful::WASM, (41_i64,));
                assert_eq!(
                    local_stateful::Client::new(fork.env(), &candidate).get(),
                    41
                );
            })
            .unwrap();
        assert_eq!(changed_candidate.cache_status(), CacheStatus::Hit);
        assert_eq!(fake.ledger_entry_reads(), reads_after_capture);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn auto_runner_replaces_captured_wasm_and_reuses_cache_offline() {
        use crate::auto::{AutoRunner, CacheStatus};

        let fake = FakeTransport::from_aquarius_snapshot();
        let path = test_bundle_path("auto-runner-replace-wasm");
        let scenario = |fork: &crate::auto::ScenarioFork<'_>| {
            let pool = fork.contract(POOL_ID);
            assert_eq!(
                fork.replace_wasm(&pool, local_stateful::WASM),
                <[u8; 32]>::from(Sha256::digest(local_stateful::WASM))
            );
            assert_eq!(local_stateful::Client::new(fork.env(), &pool).get(), 0);
        };

        let first = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .run(scenario)
            .unwrap();
        assert_eq!(first.cache_status(), CacheStatus::Created);
        let local_code_hash = Hash(Sha256::digest(local_stateful::WASM).into());
        assert!(first
            .fixture()
            .frozen_fixture()
            .ledger_snapshot()
            .ledger_entries
            .iter()
            .all(|(_, (entry, _))| !matches!(
                &entry.data,
                LedgerEntryData::ContractCode(code) if code.hash == local_code_hash
            )));
        let reads_after_capture = fake.ledger_entry_reads();

        let second = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .offline()
            .run(scenario)
            .unwrap();
        assert_eq!(second.cache_status(), CacheStatus::Hit);
        assert_eq!(fake.ledger_entry_reads(), reads_after_capture);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn preview_only_dependencies_reach_fixed_point_and_replay_offline() {
        use crate::{
            auto::{AutoRunner, CacheStatus},
            PreviewAuth,
        };

        let fake = FakeTransport::from_aquarius_snapshot();
        let path = test_bundle_path("auto-runner-preview-only");
        let scenario = |fork: &crate::auto::ScenarioFork<'_>| {
            let pool = fork.contract(POOL_ID);
            let candidate = fork.deploy(local_wrapper::WASM, (pool,));
            let report = fork
                .preview(
                    &candidate,
                    "estimate_swap",
                    (1_u32, 0_u32, ONE_USDC),
                    PreviewAuth::Record,
                )
                .unwrap();
            assert!(report.result::<u128>(fork.env()).unwrap() > 0);
        };

        let first = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .run(scenario)
            .unwrap();
        assert_eq!(first.cache_status(), CacheStatus::Created);
        assert!(first.fixture().report().present_entries() > 0);
        let reads_after_capture = fake.ledger_entry_reads();

        let second = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .offline()
            .run(scenario)
            .unwrap();
        assert_eq!(second.cache_status(), CacheStatus::Hit);
        assert_eq!(fake.ledger_entry_reads(), reads_after_capture);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn auto_runner_recaptures_unknown_scenario_keys_and_replaces_cache() {
        use crate::auto::{AutoRunner, CacheStatus};

        let fake = FakeTransport::from_aquarius_snapshot();
        let path = test_bundle_path("auto-runner-refresh");
        AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .run(|fork| mixed_auto_scenario_for_user(fork, "first-user"))
            .unwrap();
        let reads_before_refresh = fake.ledger_entry_reads();

        let refreshed = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .run(|fork| mixed_auto_scenario_for_user(fork, "second-user"))
            .unwrap();
        assert_eq!(refreshed.cache_status(), CacheStatus::Refreshed);
        assert!(fake.ledger_entry_reads() > reads_before_refresh);

        let reads_after_refresh = fake.ledger_entry_reads();
        let replayed = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .offline()
            .run(|fork| mixed_auto_scenario_for_user(fork, "second-user"))
            .unwrap();
        assert_eq!(replayed.cache_status(), CacheStatus::Hit);
        assert_eq!(fake.ledger_entry_reads(), reads_after_refresh);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn auto_runner_offline_missing_cache_fails_without_rpc_reads() {
        use crate::auto::{AutoRunError, AutoRunner};

        let fake = FakeTransport::from_aquarius_snapshot();
        let path = test_bundle_path("auto-runner-missing");
        let result = AutoRunner::with_builder(builder(&fake), MAINNET_PASSPHRASE)
            .cache(&path)
            .offline()
            .run(|_| {});

        assert!(matches!(
            result,
            Err(AutoRunError::OfflineCacheMissing { path: missing }) if missing == path
        ));
        assert_eq!(fake.ledger_entry_reads(), 0);
    }

    #[test]
    fn versioned_bundle_roundtrips_and_replays_strictly_offline() {
        let fake = FakeTransport::from_aquarius_snapshot();
        let captured = builder(&fake).capture(aquarius_scenario).unwrap();
        let path = test_bundle_path("roundtrip");
        captured.write_file(&path).unwrap();

        let loaded = CapturedFixture::from_file(&path, MAINNET_PASSPHRASE).unwrap();
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
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(document["schema_version"], 2);
        assert!(document.get("root_contract").is_none());
        document["root_contract"] = serde_json::Value::String(POOL_ID.to_string());
        fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        assert!(matches!(
            CapturedFixture::from_file(&path, MAINNET_PASSPHRASE),
            Err(CaptureError::MalformedCaptureBundle)
        ));
        document.as_object_mut().unwrap().remove("root_contract");
        document["schema_version"] = serde_json::Value::from(3);
        fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        assert!(matches!(
            CapturedFixture::from_file(&path, MAINNET_PASSPHRASE),
            Err(CaptureError::UnsupportedCaptureBundleSchema {
                found: 3,
                supported: 2
            })
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn versioned_bundle_rejects_digest_tampering() {
        let fake = FakeTransport::from_aquarius_snapshot();
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
        let mut fake = FakeTransport::from_aquarius_snapshot();
        let contract = parse_legacy_contract_address(POOL_ID).unwrap();
        let contract_id = key_id(&contract_instance_key(contract)).unwrap();
        Rc::get_mut(&mut fake)
            .unwrap()
            .entries
            .get_mut(&contract_id)
            .unwrap()
            .live_until = None;
        assert!(matches!(
            builder(&fake).capture(|env| {
                let pool_id = Address::from_str(env, POOL_ID);
                let _ = pool::Client::new(env, &pool_id).get_tokens();
            }),
            Err(CaptureError::MalformedRpc { .. })
        ));

        let captured = builder(&FakeTransport::from_aquarius_snapshot())
            .capture(aquarius_scenario)
            .unwrap();
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
        let captured = builder(&FakeTransport::from_aquarius_snapshot())
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
        let fake = FakeTransport::from_aquarius_snapshot();
        let contract = parse_legacy_contract_address(POOL_ID).unwrap();
        let missing = data_key(contract.clone(), 777);
        let write_only = data_key(contract.clone(), 778);
        let written_entry = contract_data_entry(contract, 778, 42);
        let scenario = |env: &Env| {
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

        let unknown = data_key(parse_legacy_contract_address(POOL_ID).unwrap(), 779);
        let error = captured
            .replay(|env| {
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
        let fake = FakeTransport::from_aquarius_snapshot();
        fake.mix_second_large_batch_once.set(true);
        let contract = parse_legacy_contract_address(POOL_ID).unwrap();
        let keys: Vec<LedgerKey> = (1_000..1_401)
            .map(|index| data_key(contract.clone(), index))
            .collect();

        let captured = builder(&fake)
            .capture(|env| {
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
        let fake = FakeTransport::from_aquarius_snapshot();
        let panic_error = builder(&fake)
            .capture(|_| panic!("scenario-payload-must-not-leak"))
            .unwrap_err();
        let rendered = format!("{panic_error:?} {panic_error}");
        assert!(matches!(panic_error, CaptureError::ScenarioPanicked));
        assert!(!rendered.contains("scenario-payload-must-not-leak"));

        let failed_key = data_key(parse_legacy_contract_address(POOL_ID).unwrap(), 888);
        fake.fail_keys
            .borrow_mut()
            .insert(key_id(&failed_key).unwrap());
        let transport_error = builder(&fake)
            .capture(|env| {
                let _ = env.host().get_ledger_entry(&Rc::new(failed_key.clone()));
            })
            .unwrap_err();
        assert!(matches!(transport_error, CaptureError::Transport { .. }));
    }

    #[test]
    fn generated_client_transport_panic_returns_the_capture_failure() {
        let fake = FakeTransport::from_aquarius_snapshot();
        let contract = parse_legacy_contract_address(POOL_ID).unwrap();
        let failed_key = contract_instance_key(contract);
        let failed_id = key_id(&failed_key).unwrap();
        fake.fail_keys.borrow_mut().insert(failed_id.clone());

        let error = builder(&fake)
            .capture(|env| {
                let pool_id = Address::from_str(env, POOL_ID);
                let _ = pool::Client::new(env, &pool_id).get_tokens();
            })
            .unwrap_err();

        assert!(matches!(
            error,
            CaptureError::Transport {
                operation: "getLedgerEntries"
            }
        ));
        assert_eq!(
            fake.batch_calls
                .borrow()
                .iter()
                .filter(|keys| keys.contains(&failed_id))
                .count(),
            1
        );
    }

    #[test]
    fn read_rpc_retries_transient_statuses_with_short_backoff() {
        let mut responses = VecDeque::from([
            Err(http_status_error(429, Some("0"))),
            Err(http_status_error(503, None)),
            Ok(ureq::Response::new(200, "OK", "{}").unwrap()),
        ]);
        let mut delays = Vec::new();

        let response = send_read_rpc_with_retry(
            || responses.pop_front().expect("one response per attempt"),
            |delay| delays.push(delay),
        )
        .unwrap();

        assert_eq!(response.status(), 200);
        assert_eq!(
            delays,
            [Duration::from_millis(200), Duration::from_millis(400)]
        );
        assert!(responses.is_empty());
    }

    #[test]
    fn read_rpc_retry_budget_is_bounded_and_non_retryable_statuses_fail_fast() {
        let attempts = Cell::new(0_u32);
        let mut delays = Vec::new();
        let exhausted = send_read_rpc_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                Err(http_status_error(500, None))
            },
            |delay| delays.push(delay),
        )
        .unwrap_err();

        assert!(matches!(*exhausted, ureq::Error::Status(500, _)));
        assert_eq!(attempts.get(), 4);
        assert_eq!(
            delays,
            [
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(800)
            ]
        );

        let attempts = Cell::new(0_u32);
        let mut delays = Vec::new();
        let rejected = send_read_rpc_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                Err(http_status_error(400, None))
            },
            |delay| delays.push(delay),
        )
        .unwrap_err();

        assert!(matches!(*rejected, ureq::Error::Status(400, _)));
        assert_eq!(attempts.get(), 1);
        assert!(delays.is_empty());
    }

    #[test]
    fn read_rpc_honors_bounded_numeric_retry_after() {
        let mut responses = VecDeque::from([
            Err(http_status_error(429, Some("1"))),
            Ok(ureq::Response::new(200, "OK", "{}").unwrap()),
        ]);
        let mut delays = Vec::new();

        send_read_rpc_with_retry(
            || responses.pop_front().expect("one response per attempt"),
            |delay| delays.push(delay),
        )
        .unwrap();

        assert_eq!(delays, [Duration::from_secs(1)]);

        let attempts = Cell::new(0_u32);
        let mut delays = Vec::new();
        let rejected = send_read_rpc_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                Err(http_status_error(429, Some("6")))
            },
            |delay| delays.push(delay),
        )
        .unwrap_err();

        assert!(matches!(*rejected, ureq::Error::Status(429, _)));
        assert_eq!(attempts.get(), 1);
        assert!(delays.is_empty());
    }

    #[test]
    fn request_rate_limiter_spaces_requests_and_can_be_disabled() {
        let limiter = RequestRateLimiter::new(5);
        let start = std::time::Instant::now();

        assert_eq!(limiter.reserve_delay(start), Duration::ZERO);
        assert_eq!(
            limiter.reserve_delay(start + Duration::from_millis(50)),
            Duration::from_millis(150)
        );
        assert_eq!(
            limiter.reserve_delay(start + Duration::from_millis(400)),
            Duration::ZERO
        );

        limiter.set_requests_per_second(0);
        assert_eq!(limiter.reserve_delay(start), Duration::ZERO);
        assert_eq!(limiter.reserve_delay(start), Duration::ZERO);
    }

    #[test]
    fn tracking_source_latches_transport_failure_without_refetching() {
        let fake = FakeTransport::from_aquarius_snapshot();
        let failed_key = data_key(parse_legacy_contract_address(POOL_ID).unwrap(), 889);
        fake.fail_keys
            .borrow_mut()
            .insert(key_id(&failed_key).unwrap());
        let source = TrackingSource::rpc_with_local(
            BTreeMap::new(),
            fake.clone(),
            Rc::new(LocalLedger::default()),
        );
        let failed_key = Rc::new(failed_key);

        assert!(source.get(&failed_key).is_err());
        assert!(source.get(&failed_key).is_err());

        assert_eq!(fake.ledger_entry_reads(), 1);
        assert_eq!(source.take_failure(), Some("getLedgerEntries"));
    }

    fn http_status_error(status: u16, retry_after: Option<&str>) -> ureq::Error {
        let retry_after = retry_after
            .map(|value| format!("Retry-After: {value}\r\n"))
            .unwrap_or_default();
        let raw = format!("HTTP/1.1 {status} Test\r\n{retry_after}\r\n");
        ureq::Error::Status(status, raw.parse().unwrap())
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
    fn testnet_builder_uses_testnet_identity_and_redacts_rpc_origin() {
        let builder = CaptureBuilder::testnet(
            "https://user:secret@testnet-rpc.example.test/private?token=secret",
        )
        .unwrap();

        assert_eq!(builder.network_passphrase, TESTNET_PASSPHRASE);
        assert_eq!(builder.rpc_origin, "https://testnet-rpc.example.test");
        let rendered = format!("{builder:?}");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("private"));
    }

    #[test]
    fn capture_fails_closed_when_provider_network_differs_from_selected_network() {
        let fake = FakeTransport::from_aquarius_snapshot();
        let error = CaptureBuilder::with_transport(fake, TESTNET_PASSPHRASE)
            .capture(|_| {})
            .unwrap_err();

        assert!(matches!(error, CaptureError::NetworkMismatch));
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
        let contract = parse_legacy_contract_address(POOL_ID).unwrap();
        let mut persistent = contract_data_entry(contract, 1, 1);
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

    fn aquarius_scenario(env: &Env) {
        let pool_id = Address::from_str(env, POOL_ID);
        let pool = pool::Client::new(env, &pool_id);
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
            args: (
                user.clone(),
                pool_id.clone(),
                i128::try_from(amount).unwrap(),
            )
                .into_val(env),
            sub_invokes: &[],
        };
        let children = [transfer];
        let swap = MockAuthInvoke {
            contract: &pool_id,
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

    fn mixed_auto_scenario(fork: &crate::auto::ScenarioFork<'_>) {
        mixed_auto_scenario_for_user(fork, "swap-user");
    }

    fn mixed_auto_scenario_for_user(fork: &crate::auto::ScenarioFork<'_>, user_label: &str) {
        let env = fork.env();
        let user = fork.local_account(user_label);
        fork.fund_local_account(&user, 100_000_000);
        let pool_id = fork.contract(POOL_ID);
        let usdc = fork.contract(USDC_ID);
        let pool = pool::Client::new(env, &pool_id);
        assert!(matches!(ScAddress::from(&user), ScAddress::Account(_)));

        assert_eq!(pool.get_tokens().get(1).unwrap(), usdc);
        let before = pool.estimate_swap(&1, &0, &ONE_USDC);
        let reserves = pool.get_reserves();
        let amount = reserves.get(1).unwrap() / 10;
        let amount_i128 = i128::try_from(amount).unwrap();

        fork.mock_all_auths();
        let admin = fork.invoke::<Address>(&usdc, "admin", ());
        fork.invoke::<()>(&usdc, "trust", (user.clone(),));
        assert_eq!(fork.local_account(user_label), user);
        fork.invoke::<()>(&usdc, "set_authorized", (user.clone(), true));
        fork.invoke::<()>(&usdc, "mint", (user.clone(), amount_i128));
        assert_eq!(env.auths()[0].0, admin);
        let received = fork.invoke::<u128>(&pool_id, "swap", (user, 1_u32, 0_u32, amount, 0_u128));
        assert!(received > 0);

        let after = pool.estimate_swap(&1, &0, &ONE_USDC);
        assert!(after < before);
    }

    fn test_bundle_path(label: &str) -> PathBuf {
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "kanatoko-{label}-{}-{sequence}.capture.json",
            std::process::id()
        ))
    }

    fn refresh_bundle_digests(bundle: &mut CaptureBundleV2) {
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
            &bundle.known_absent,
            &bundle.report,
            &bundle.provenance,
            &fixture.ledger_digest(),
            &inventory,
        )
        .unwrap());
    }

    fn builder(fake: &Rc<FakeTransport>) -> CaptureBuilder {
        CaptureBuilder::with_transport(fake.clone(), MAINNET_PASSPHRASE)
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
        fn from_aquarius_snapshot() -> Rc<Self> {
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
