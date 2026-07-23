//! Strict, mutable execution over a coherent captured fixture.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    ContractDataDurability, ContractEvent, ContractEventType, LedgerEntry, LedgerKey,
    LedgerKeyContractData, Limits, ScAddress, ScError, ScSymbol, ScVal, SorobanAuthorizationEntry,
    SorobanAuthorizedInvocation, WriteXdr,
};
use soroban_ledger_snapshot::LedgerSnapshot;
use soroban_sdk::{
    testutils::{EnvTestConfig, HostError, SnapshotSource, SnapshotSourceInput},
    Env, Error, InvokeError, Symbol, TryFromVal, Val, Vec as SorobanVec,
};
use thiserror::Error;

use crate::{
    canonical_ledger_digest,
    capture::{KeyId, LookupState},
    FixtureError,
};

static NEXT_STRICT_FORK_ID: AtomicU64 = AtomicU64::new(1);

/// A detached authorization tree observed by the Host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationTree {
    pub address: ScAddress,
    pub invocation: SorobanAuthorizedInvocation,
}

/// Authorization policy applied to one invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthMode {
    /// SDK recording mode. It also mock-satisfies the auth it discovers.
    Record,
    /// Record in an isolated child and accept the invocation only if the exact
    /// detached tree equals this value.
    MockExact(Vec<AuthorizationTree>),
    /// Host enforcement with supplied Stellar authorization entries.
    Enforce(Vec<SorobanAuthorizationEntry>),
}

/// Honest authorization evidence attached to a receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppliedAuthMode {
    RecordMockSatisfied,
    MockExact,
    Enforce,
}

/// Whether an invocation is simulated or eligible to update the fork.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionMode {
    Preview,
    Apply,
}

/// Fully detached generic contract invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvokeRequest {
    pub contract: ScAddress,
    pub function: ScSymbol,
    pub args: Vec<ScVal>,
}

/// Coarse fallback when an SDK invocation error cannot be represented as an
/// exact [`ScError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvokeErrorKind {
    Abort,
    Contract(u32),
    ResultConversion,
}

/// Stable, structured invocation failure.
///
/// Kanatoko never derives this value by parsing panic messages or diagnostic
/// text. `error` preserves the Host's typed [`ScError`] when one is available;
/// `kind` is the deliberately small fallback taxonomy.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("contract invocation failed")]
pub struct InvocationFailure {
    pub error: Option<ScError>,
    pub kind: InvokeErrorKind,
}

/// Detached success or failure returned by the generic invoke API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvokeOutcome {
    Success(ScVal),
    Failure {
        error: Option<ScError>,
        kind: InvokeErrorKind,
    },
}

impl InvokeOutcome {
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }
}

/// A detached contract or diagnostic event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachedEvent {
    pub event: ContractEvent,
    pub failed_call: bool,
}

/// One detached ledger value in a receipt diff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerValue {
    pub entry: LedgerEntry,
    pub live_until_ledger: Option<u32>,
}

/// Exact before/after value for one canonical ledger key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateChange {
    pub key: LedgerKey,
    pub before: Option<LedgerValue>,
    pub after: Option<LedgerValue>,
}

/// Evidence level for installing candidate code locally.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateInstallMode {
    /// SDK test-only `register_at` injection with constructor execution. This
    /// is contract-functional evidence, not a transaction-faithful deploy.
    LocalInjection,
}

/// Detached result of a successful local candidate installation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRegistration {
    pub address: ScAddress,
    pub wasm_sha256: [u8; 32],
    pub mode: CandidateInstallMode,
    pub state_changes: Vec<StateChange>,
    pub before_digest: [u8; 32],
    pub after_digest: [u8; 32],
    pub upstream_reads: u64,
}

/// Whether the isolated child was discarded or promoted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptDisposition {
    Previewed,
    Committed,
    Rejected,
}

/// Detached evidence for one local Host invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Receipt {
    pub request: InvokeRequest,
    pub outcome: InvokeOutcome,
    pub auth_mode: AppliedAuthMode,
    pub authorization: Vec<AuthorizationTree>,
    pub events: Vec<DetachedEvent>,
    pub diagnostics: Vec<DetachedEvent>,
    pub state_changes: Vec<StateChange>,
    pub before_digest: [u8; 32],
    pub after_digest: [u8; 32],
    pub disposition: ReceiptDisposition,
    pub upstream_reads: u64,
}

/// Strict ledger state and local allowlists captured for revert.
#[derive(Clone, Debug)]
pub struct StrictCheckpoint {
    fork_id: u64,
    snapshot: LedgerSnapshot,
    coverage: BTreeMap<KeyId, LookupState>,
    local_contracts: BTreeSet<ScAddress>,
    local_code_hashes: BTreeSet<[u8; 32]>,
}

/// A stateful offline fork whose omitted keys remain Unknown rather than being
/// silently treated as absent.
pub struct StrictFork {
    id: u64,
    env: Env,
    coverage: BTreeMap<KeyId, LookupState>,
    local_contracts: BTreeSet<ScAddress>,
    local_code_hashes: BTreeSet<[u8; 32]>,
    receipts: Vec<Receipt>,
}

impl StrictFork {
    pub(crate) fn from_captured(
        snapshot: &LedgerSnapshot,
        coverage: BTreeMap<KeyId, LookupState>,
    ) -> Self {
        let id = NEXT_STRICT_FORK_ID.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, 0, "Kanatoko strict fork ID space exhausted");
        let (env, _) = strict_env(
            snapshot,
            &coverage,
            &BTreeSet::new(),
            &BTreeSet::new(),
            false,
        );
        Self {
            id,
            env,
            coverage,
            local_contracts: BTreeSet::new(),
            local_code_hashes: BTreeSet::new(),
            receipts: Vec::new(),
        }
    }

    /// Strict forks never retain an RPC transport.
    #[must_use]
    pub const fn upstream_reads(&self) -> u64 {
        0
    }

    /// Canonical digest of current ledger state.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError`] if the Host snapshot cannot be encoded into
    /// the canonical detached digest.
    pub fn ledger_digest(&self) -> Result<[u8; 32], FixtureError> {
        canonical_ledger_digest(&self.env.to_ledger_snapshot())
    }

    /// Receipts remain detached from every replaced `Env`.
    #[must_use]
    pub fn receipts(&self) -> &[Receipt] {
        &self.receipts
    }

    /// Captures strict ledger/coverage state. SDK address/nonce generator
    /// continuity is deliberately not claimed for strict forks under the
    /// selected SDK.
    #[must_use]
    pub fn checkpoint(&self) -> StrictCheckpoint {
        StrictCheckpoint {
            fork_id: self.id,
            snapshot: self.env.to_ledger_snapshot(),
            coverage: self.coverage.clone(),
            local_contracts: self.local_contracts.clone(),
            local_code_hashes: self.local_code_hashes.clone(),
        }
    }

    /// Replaces the owned environment and clears receipt history. All prior
    /// SDK clients and values are stale; this API intentionally returns only
    /// detached XDR values.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::CheckpointMismatch`] when the checkpoint
    /// came from another fork.
    pub fn revert(&mut self, checkpoint: StrictCheckpoint) -> Result<(), StrictForkError> {
        if checkpoint.fork_id != self.id {
            return Err(StrictForkError::CheckpointMismatch);
        }
        let (env, _) = strict_env(
            &checkpoint.snapshot,
            &checkpoint.coverage,
            &checkpoint.local_contracts,
            &checkpoint.local_code_hashes,
            false,
        );
        self.env = env;
        self.coverage = checkpoint.coverage;
        self.local_contracts = checkpoint.local_contracts;
        self.local_code_hashes = checkpoint.local_code_hashes;
        self.receipts.clear();
        Ok(())
    }

    /// Hash-validates and locally injects production WASM at a detached
    /// contract address, executing its constructor in an isolated strict
    /// child. This deliberately uses the SDK test-only registration path and
    /// does not emulate upload/create transaction semantics.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError`] when the hash/address is invalid, the
    /// constructor fails, or installation touches an uncaptured ledger key.
    pub fn register_candidate(
        &mut self,
        address: ScAddress,
        wasm: &[u8],
        expected_sha256: [u8; 32],
        constructor_args: Vec<ScVal>,
    ) -> Result<CandidateRegistration, StrictForkError> {
        if !matches!(address, ScAddress::Contract(_)) {
            return Err(StrictForkError::CandidateAddressNotContract);
        }
        let actual_sha256: [u8; 32] = Sha256::digest(wasm).into();
        if actual_sha256 != expected_sha256 {
            return Err(StrictForkError::WasmHashMismatch {
                expected: expected_sha256,
                actual: actual_sha256,
            });
        }

        let instance_key = contract_instance_key(address.clone());
        if matches!(
            self.coverage.get(&key_id(&instance_key)?),
            Some(LookupState::Present(_, _))
        ) {
            return Err(StrictForkError::CandidateAddressOccupied);
        }

        let before = self.env.to_ledger_snapshot();
        let before_digest = canonical_ledger_digest(&before)?;
        let mut local_contracts = self.local_contracts.clone();
        local_contracts.insert(address.clone());
        let mut local_code_hashes = self.local_code_hashes.clone();
        local_code_hashes.insert(actual_sha256);
        let (child, source) = strict_env(
            &before,
            &self.coverage,
            &local_contracts,
            &local_code_hashes,
            false,
        );
        let address_value = AddressValue::from_xdr(&child, &address)?;
        let args = constructor_args
            .into_iter()
            .map(|arg| Val::try_from_val(&child, &arg))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StrictForkError::InvalidInvocationXdr)?;
        let args = SorobanVec::from_iter(&child, args);

        let installed = catch_unwind(AssertUnwindSafe(|| {
            child.register_at(&address_value.0, wasm, args)
        }));

        let unknown = unknown_keys(
            &child,
            &source,
            &self.coverage,
            &local_contracts,
            &local_code_hashes,
            false,
        )?;
        if !unknown.is_empty() {
            return Err(StrictForkError::UnknownLedgerKeys {
                count: unknown.len(),
                keys: unknown.into_iter().collect(),
            });
        }
        let installed = installed.map_err(|_| StrictForkError::CandidateRegistrationFailed)?;
        let installed: ScAddress = (&installed).into();
        if installed != address {
            return Err(StrictForkError::CandidateRegistrationFailed);
        }

        let after = child.to_ledger_snapshot();
        let after_digest = canonical_ledger_digest(&after)?;
        let state_changes = state_diff(&before, &after)?;
        update_coverage(&mut self.coverage, &state_changes)?;
        self.local_contracts = local_contracts;
        self.local_code_hashes = local_code_hashes;
        self.env = child;

        Ok(CandidateRegistration {
            address,
            wasm_sha256: actual_sha256,
            mode: CandidateInstallMode::LocalInjection,
            state_changes,
            before_digest,
            after_digest,
            upstream_reads: 0,
        })
    }

    /// Executes a generic invocation in an isolated strict child. Only a
    /// successful, validated [`ExecutionMode::Apply`] promotes the child.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError`] for invalid detached XDR, exact-auth
    /// mismatch, Host inspection failure, or any uncaptured ledger access.
    pub fn invoke(
        &mut self,
        request: InvokeRequest,
        execution: ExecutionMode,
        auth: AuthMode,
    ) -> Result<Receipt, StrictForkError> {
        let before = self.env.to_ledger_snapshot();
        let before_digest = canonical_ledger_digest(&before)?;
        let allow_mock_nonces = matches!(&auth, AuthMode::Record | AuthMode::MockExact(_));
        let (child, source) = strict_env(
            &before,
            &self.coverage,
            &self.local_contracts,
            &self.local_code_hashes,
            allow_mock_nonces,
        );

        let (contract, function, args) = invocation_values(&child, &request)?;
        let (applied_auth, expected_auth) = match auth {
            AuthMode::Record => {
                child.mock_all_auths();
                (AppliedAuthMode::RecordMockSatisfied, None)
            }
            AuthMode::MockExact(expected) => {
                child.mock_all_auths();
                (AppliedAuthMode::MockExact, Some(expected))
            }
            AuthMode::Enforce(entries) => {
                child
                    .host()
                    .set_authorization_entries(entries)
                    .map_err(|_| StrictForkError::InvalidAuthorizationEntries)?;
                (AppliedAuthMode::Enforce, None)
            }
        };
        let outcome = invoke_outcome(&child, &contract, &function, args);
        let unknown = unknown_keys(
            &child,
            &source,
            &self.coverage,
            &self.local_contracts,
            &self.local_code_hashes,
            allow_mock_nonces,
        )?;
        if !unknown.is_empty() {
            return Err(StrictForkError::UnknownLedgerKeys {
                count: unknown.len(),
                keys: unknown.into_iter().collect(),
            });
        }

        let (authorization, events) = detached_snapshot_evidence(&child);
        if expected_auth
            .as_ref()
            .is_some_and(|expected| expected != &authorization)
        {
            return Err(StrictForkError::AuthTreeMismatch);
        }

        let mut after = child.to_ledger_snapshot();
        if allow_mock_nonces {
            strip_new_mock_nonces(&before, &mut after)?;
        }
        let after_digest = canonical_ledger_digest(&after)?;
        let state_changes = state_diff(&before, &after)?;
        let diagnostics = detached_diagnostics(&child)?;

        let commit = execution == ExecutionMode::Apply && outcome.is_success();
        let disposition = match (execution, commit) {
            (ExecutionMode::Preview, _) => ReceiptDisposition::Previewed,
            (ExecutionMode::Apply, true) => ReceiptDisposition::Committed,
            (ExecutionMode::Apply, false) => ReceiptDisposition::Rejected,
        };
        let receipt = Receipt {
            request,
            outcome,
            auth_mode: applied_auth,
            authorization,
            events,
            diagnostics,
            state_changes,
            before_digest,
            after_digest,
            disposition,
            upstream_reads: 0,
        };

        if commit {
            let mut coverage = self.coverage.clone();
            update_coverage(&mut coverage, &receipt.state_changes)?;
            let (promoted, _) = strict_env(
                &after,
                &coverage,
                &self.local_contracts,
                &self.local_code_hashes,
                false,
            );
            self.coverage = coverage;
            self.env = promoted;
        }
        self.receipts.push(receipt.clone());
        Ok(receipt)
    }
}

pub(crate) fn invocation_values(
    env: &Env,
    request: &InvokeRequest,
) -> Result<(AddressValue, Symbol, SorobanVec<Val>), StrictForkError> {
    let contract = AddressValue::from_xdr(env, &request.contract)?;
    let function = Symbol::try_from_val(env, &ScVal::Symbol(request.function.clone()))
        .map_err(|_| StrictForkError::InvalidInvocationXdr)?;
    let args = request
        .args
        .iter()
        .map(|arg| Val::try_from_val(env, arg))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StrictForkError::InvalidInvocationXdr)?;
    Ok((contract, function, SorobanVec::from_iter(env, args)))
}

pub(crate) fn invoke_outcome(
    env: &Env,
    contract: &AddressValue,
    function: &Symbol,
    args: SorobanVec<Val>,
) -> InvokeOutcome {
    match invoke_once::<ScVal>(env, &contract.0, function, args) {
        Ok(value) => InvokeOutcome::Success(value),
        Err(InvocationFailure { error, kind }) => InvokeOutcome::Failure { error, kind },
    }
}

pub(crate) fn invoke_once<R>(
    env: &Env,
    contract: &soroban_sdk::Address,
    function: &Symbol,
    args: SorobanVec<Val>,
) -> Result<R, InvocationFailure>
where
    R: TryFromVal<Env, Val>,
{
    let call = catch_unwind(AssertUnwindSafe(|| {
        env.try_invoke_contract::<R, Error>(contract, function, args)
    }));
    match call {
        Err(_) | Ok(Err(Err(InvokeError::Abort))) => Err(InvocationFailure {
            error: None,
            kind: InvokeErrorKind::Abort,
        }),
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(_))) => Err(InvocationFailure {
            error: None,
            kind: InvokeErrorKind::ResultConversion,
        }),
        Ok(Err(Ok(error))) => Err(InvocationFailure {
            error: ScError::try_from(error).ok(),
            kind: if error.is_type(soroban_env_host::xdr::ScErrorType::Contract) {
                InvokeErrorKind::Contract(error.get_code())
            } else {
                InvokeErrorKind::Abort
            },
        }),
        Ok(Err(Err(InvokeError::Contract(code)))) => Err(InvocationFailure {
            error: Some(ScError::Contract(code)),
            kind: InvokeErrorKind::Contract(code),
        }),
    }
}

pub(crate) fn detached_snapshot_evidence(
    env: &Env,
) -> (Vec<AuthorizationTree>, Vec<DetachedEvent>) {
    let snapshot = env.to_snapshot();
    let authorization = snapshot
        .auth
        .0
        .last()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|(address, invocation)| AuthorizationTree {
            address,
            invocation,
        })
        .collect();
    let events = snapshot
        .events
        .0
        .into_iter()
        .map(|event| DetachedEvent {
            event: event.event,
            failed_call: event.failed_call,
        })
        .collect();
    (authorization, events)
}

pub(crate) fn detached_diagnostics(env: &Env) -> Result<Vec<DetachedEvent>, StrictForkError> {
    Ok(env
        .host()
        .get_diagnostic_events()
        .map_err(|_| StrictForkError::HostInspection)?
        .0
        .into_iter()
        .filter(|event| event.event.type_ == ContractEventType::Diagnostic)
        .map(|event| DetachedEvent {
            event: event.event,
            failed_call: event.failed_call,
        })
        .collect())
}

pub(crate) struct AddressValue(pub(crate) soroban_sdk::Address);

impl AddressValue {
    fn from_xdr(env: &Env, address: &ScAddress) -> Result<Self, StrictForkError> {
        soroban_sdk::Address::try_from_val(env, address)
            .map(Self)
            .map_err(|_| StrictForkError::InvalidInvocationXdr)
    }
}

#[derive(Clone)]
struct StrictSource {
    coverage: BTreeMap<KeyId, LookupState>,
    local_contracts: BTreeSet<ScAddress>,
    local_code_hashes: BTreeSet<[u8; 32]>,
    allow_mock_nonces: bool,
    unknown: Rc<RefCell<BTreeSet<KeyId>>>,
}

impl SnapshotSource for StrictSource {
    fn get(
        &self,
        key: &Rc<LedgerKey>,
    ) -> Result<Option<(Rc<LedgerEntry>, Option<u32>)>, HostError> {
        let id = key_id(key).map_err(|_| host_storage_error())?;
        if let Some(state) = self.coverage.get(&id) {
            return Ok(match state {
                LookupState::Present(entry, live_until) => Some((entry.clone(), *live_until)),
                LookupState::Absent => None,
            });
        }
        if is_local_key(
            key,
            &self.local_contracts,
            &self.local_code_hashes,
            self.allow_mock_nonces,
        ) {
            return Ok(None);
        }
        self.unknown.borrow_mut().insert(id);
        // The isolated child may continue down an "absent" branch so that we
        // can collect complete diagnostics, but it is never promoted: the
        // post-invocation unknown-key gate below fails closed atomically.
        Ok(None)
    }
}

fn strict_env(
    snapshot: &LedgerSnapshot,
    coverage: &BTreeMap<KeyId, LookupState>,
    local_contracts: &BTreeSet<ScAddress>,
    local_code_hashes: &BTreeSet<[u8; 32]>,
    allow_mock_nonces: bool,
) -> (Env, Rc<StrictSource>) {
    let source = Rc::new(StrictSource {
        coverage: coverage.clone(),
        local_contracts: local_contracts.clone(),
        local_code_hashes: local_code_hashes.clone(),
        allow_mock_nonces,
        unknown: Rc::new(RefCell::new(BTreeSet::new())),
    });
    let mut env = Env::from_ledger_snapshot(SnapshotSourceInput {
        source: source.clone(),
        ledger_info: Some(snapshot.ledger_info()),
        snapshot: Some(Rc::new(snapshot.clone())),
    });
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    (env, source)
}

fn unknown_keys(
    env: &Env,
    source: &StrictSource,
    coverage: &BTreeMap<KeyId, LookupState>,
    local_contracts: &BTreeSet<ScAddress>,
    local_code_hashes: &BTreeSet<[u8; 32]>,
    allow_mock_nonces: bool,
) -> Result<BTreeSet<KeyId>, StrictForkError> {
    let mut unknown = source.unknown.borrow().clone();
    for (key, _) in env
        .host()
        .get_stored_entries()
        .map_err(|_| StrictForkError::HostInspection)?
    {
        let id = key_id(&key)?;
        if !coverage.contains_key(&id)
            && !is_local_key(&key, local_contracts, local_code_hashes, allow_mock_nonces)
        {
            unknown.insert(id);
        }
    }
    Ok(unknown)
}

fn is_local_key(
    key: &LedgerKey,
    local_contracts: &BTreeSet<ScAddress>,
    local_code_hashes: &BTreeSet<[u8; 32]>,
    allow_mock_nonces: bool,
) -> bool {
    match key {
        LedgerKey::ContractData(data) => {
            local_contracts.contains(&data.contract) || (allow_mock_nonces && is_nonce_data(data))
        }
        LedgerKey::ContractCode(code) => local_code_hashes.contains(&code.hash.0),
        _ => false,
    }
}

pub(crate) fn is_nonce_data(data: &LedgerKeyContractData) -> bool {
    data.durability == ContractDataDurability::Temporary
        && matches!(data.key, ScVal::LedgerKeyNonce(_))
}

pub(crate) fn strip_new_mock_nonces(
    before: &LedgerSnapshot,
    after: &mut LedgerSnapshot,
) -> Result<(), StrictForkError> {
    // Recording auth consumes deterministic temporary nonces to model a real
    // authorization footprint. Record/MockExact are explicitly mock-satisfied,
    // so those newly generated anti-replay entries are simulation scaffolding,
    // not state that this fork may present as signed authorization evidence.
    let before_nonces = before
        .ledger_entries
        .iter()
        .filter_map(|(key, _)| match key.as_ref() {
            LedgerKey::ContractData(data) if is_nonce_data(data) => Some(key_id(key)),
            _ => None,
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut filtered = Vec::with_capacity(after.ledger_entries.len());
    for entry in std::mem::take(&mut after.ledger_entries) {
        let keep = match entry.0.as_ref() {
            LedgerKey::ContractData(data) if is_nonce_data(data) => {
                before_nonces.contains(&key_id(&entry.0)?)
            }
            _ => true,
        };
        if keep {
            filtered.push(entry);
        }
    }
    after.ledger_entries = filtered;
    Ok(())
}

fn contract_instance_key(contract: ScAddress) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract,
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    })
}

fn key_id(key: &LedgerKey) -> Result<KeyId, StrictForkError> {
    key.to_xdr(Limits::none()).map_err(|_| StrictForkError::Xdr)
}

fn host_storage_error() -> HostError {
    use soroban_env_host::xdr::{ScErrorCode, ScErrorType};
    HostError::from((ScErrorType::Storage, ScErrorCode::InternalError))
}

fn snapshot_map(
    snapshot: &LedgerSnapshot,
) -> Result<BTreeMap<KeyId, (LedgerKey, LedgerValue)>, StrictForkError> {
    snapshot
        .ledger_entries
        .iter()
        .map(|(key, (entry, live_until))| {
            Ok((
                key_id(key)?,
                (
                    (**key).clone(),
                    LedgerValue {
                        entry: (**entry).clone(),
                        live_until_ledger: *live_until,
                    },
                ),
            ))
        })
        .collect()
}

pub(crate) fn state_diff(
    before: &LedgerSnapshot,
    after: &LedgerSnapshot,
) -> Result<Vec<StateChange>, StrictForkError> {
    let before = snapshot_map(before)?;
    let after = snapshot_map(after)?;
    let ids = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut changes = Vec::new();
    for id in ids {
        let before_value = before.get(&id);
        let after_value = after.get(&id);
        if before_value.map(|(_, value)| value) == after_value.map(|(_, value)| value) {
            continue;
        }
        let key = before_value
            .map(|(key, _)| key)
            .or_else(|| after_value.map(|(key, _)| key))
            .ok_or(StrictForkError::HostInspection)?
            .clone();
        changes.push(StateChange {
            key,
            before: before_value.map(|(_, value)| value.clone()),
            after: after_value.map(|(_, value)| value.clone()),
        });
    }
    Ok(changes)
}

fn update_coverage(
    coverage: &mut BTreeMap<KeyId, LookupState>,
    changes: &[StateChange],
) -> Result<(), StrictForkError> {
    for change in changes {
        let id = key_id(&change.key)?;
        let state = change.after.as_ref().map_or(LookupState::Absent, |value| {
            LookupState::Present(Rc::new(value.entry.clone()), value.live_until_ledger)
        });
        coverage.insert(id, state);
    }
    Ok(())
}

/// Fail-closed errors from strict local execution.
#[derive(Debug, Error)]
pub enum StrictForkError {
    #[error("strict fork touched {count} unknown ledger key(s)")]
    UnknownLedgerKeys { count: usize, keys: Vec<Vec<u8>> },
    #[error("mock-exact authorization tree did not match")]
    AuthTreeMismatch,
    #[error("checkpoint belongs to another strict fork")]
    CheckpointMismatch,
    #[error("candidate address must be a contract address")]
    CandidateAddressNotContract,
    #[error("candidate address already has a present contract instance")]
    CandidateAddressOccupied,
    #[error("candidate WASM SHA-256 mismatch")]
    WasmHashMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },
    #[error("local candidate registration or constructor failed")]
    CandidateRegistrationFailed,
    #[error("detached invocation contains invalid XDR for this Host")]
    InvalidInvocationXdr,
    #[error("supplied authorization entries are invalid for this Host")]
    InvalidAuthorizationEntries,
    #[error("Host ledger/event inspection failed")]
    HostInspection,
    #[error("ledger XDR encoding failed")]
    Xdr,
    #[error(transparent)]
    Fixture(#[from] FixtureError),
}

#[cfg(test)]
mod tests {
    use soroban_env_host::xdr::{ContractId, Hash, ScNonceKey};

    use super::*;

    #[test]
    fn enforce_nonce_is_unknown_while_mock_nonce_is_explicitly_local() {
        let key = Rc::new(LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([0x71; 32]))),
            key: ScVal::LedgerKeyNonce(ScNonceKey { nonce: 9 }),
            durability: ContractDataDurability::Temporary,
        }));
        let id = key_id(&key).unwrap();

        let enforce = source(BTreeMap::new(), false);
        assert!(enforce.get(&key).unwrap().is_none());
        assert!(enforce.unknown.borrow().contains(&id));

        let mocked = source(BTreeMap::new(), true);
        assert!(mocked.get(&key).unwrap().is_none());
        assert!(mocked.unknown.borrow().is_empty());

        let mut known_absent = BTreeMap::new();
        known_absent.insert(id, LookupState::Absent);
        let enforce_known_absent = source(known_absent, false);
        assert!(enforce_known_absent.get(&key).unwrap().is_none());
        assert!(enforce_known_absent.unknown.borrow().is_empty());
    }

    fn source(coverage: BTreeMap<KeyId, LookupState>, allow_mock_nonces: bool) -> StrictSource {
        StrictSource {
            coverage,
            local_contracts: BTreeSet::new(),
            local_code_hashes: BTreeSet::new(),
            allow_mock_nonces,
            unknown: Rc::new(RefCell::new(BTreeSet::new())),
        }
    }
}
