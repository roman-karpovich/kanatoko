//! Transaction-faithful execution on a [`StrictFork`].
//!
//! [`StrictFork::invoke`] runs one contract call in an SDK test environment.
//! The methods in this module instead treat a Soroban host function as one
//! `InvokeHostFunctionOp` transaction:
//!
//! - simulation and preparation run the preflight library behind Stellar RPC
//!   `simulateTransaction` (`soroban-simulation`) against the fork's current
//!   ledger, with its default resource and fee adjustments;
//! - apply validates, charges, and executes the transaction as Stellar Core
//!   does at Protocol 29 in the next ledger, through the Host's enforcing
//!   entry point.

mod config;
#[cfg(test)]
mod tests;
mod validate;

use std::{collections::BTreeMap, fmt, rc::Rc};

use ed25519_dalek::{Signer as _, SigningKey};
use sha2::{Digest, Sha256};
use soroban_env_host::{
    budget::Budget,
    e2e_invoke::{
        entry_size_for_rent, extract_rent_changes, invoke_host_function, LedgerEntryChange,
        RecordingInvocationAuthMode, TtlLedgerEntryMeta,
    },
    xdr::{
        AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext,
        AccountEntryExtensionV2, AccountEntryExtensionV2Ext, AccountEntryExtensionV3, AccountId,
        ContractDataDurability, ContractDataEntry, ContractEvent, ContractEventType,
        ContractExecutable, ContractId, ContractIdPreimage, ContractIdPreimageFromAddress,
        CreateContractArgsV2, DecoratedSignature, DiagnosticEvent, ExtensionPoint, Hash,
        HashIdPreimage, HashIdPreimageContractId, HashIdPreimageSorobanAuthorization,
        HashIdPreimageSorobanAuthorizationWithAddress, HostFunction, InvokeContractArgs,
        InvokeHostFunctionOp, InvokeHostFunctionResultCode, LedgerEntry, LedgerEntryData,
        LedgerEntryExt, LedgerFootprint, LedgerKey, LedgerKeyAccount, LedgerKeyContractData,
        Liabilities, Limits, Memo, MuxedAccount, MuxedAccountMed25519, Operation, OperationBody,
        Preconditions, PublicKey, ReadXdr, ScAddress, ScBytes, ScError, ScErrorCode, ScErrorType,
        ScMap, ScMapEntry, ScSymbol, ScVal, ScVec, SequenceNumber, Signature, SignatureHint,
        SorobanAddressCredentials, SorobanAuthorizationEntry, SorobanCredentials, SorobanResources,
        SorobanTransactionData, SorobanTransactionDataExt, String32, Thresholds, TimeBounds,
        TimePoint, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope,
        Uint256, VecM, WriteXdr,
    },
    Host, HostError, LedgerInfo, ModuleCache,
};
use soroban_ledger_snapshot::LedgerSnapshot;
use soroban_simulation::{
    simulation::{simulate_invoke_host_function_op, SimulationAdjustmentConfig},
    NetworkConfig as PreflightConfig,
};
use thiserror::Error;

pub(crate) use config::network_config_keys;
pub use config::NetworkConfig;
use validate::{
    available_balance, check_entry_size, check_events, check_soroban_resources, check_time_bounds,
    check_writes, footprint_counts, is_soroban_key, restored_indices,
};

use crate::{
    canonical_ledger_digest,
    capture::{KeyId, LookupState},
    strict::{is_local_key, key_id, state_diff, strict_env, update_coverage, StrictSource},
    AuthorizationTree, DetachedEvent, InvokeErrorKind, InvokeOutcome, InvokeRequest, LedgerValue,
    Receipt, ReceiptDisposition, StateChange, StrictFork, StrictForkError,
};

const PRNG_SEED_DOMAIN: &[u8] = b"KANATOKO\0TRANSACTION-PRNG\0V1\0";
const SIGNER_LABEL_DOMAIN: &[u8] = b"kanatoko test signer ";
/// Network base fee per operation in stroops, on Mainnet and testnet.
/// Kanatoko does not capture the ledger header's base fee.
const BASE_FEE: i64 = 100;
/// The inclusion fee the Stellar SDKs bid by default.
const DEFAULT_INCLUSION_FEE: u32 = 100;
const MAX_SIGNATURES: u8 = 20;

/// An Ed25519 key that signs Soroban authorization entries and names a
/// transaction source account.
///
/// `Debug` output shows only the account.
#[derive(Clone)]
pub struct Ed25519Signer {
    key: SigningKey,
}

impl Ed25519Signer {
    /// Creates a signer from a 32-byte Ed25519 secret seed.
    ///
    /// Use test seeds only; a key that controls a real account must never be
    /// placed in a test. Simple seeds such as `[1; 32]` are well known and
    /// already have accounts on Mainnet; prefer [`Self::from_label`].
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// A deterministic test signer whose seed is the SHA-256 of
    /// `"kanatoko test signer "` followed by `label`.
    #[must_use]
    pub fn from_label(label: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(SIGNER_LABEL_DOMAIN);
        digest.update(label.as_bytes());
        Self::from_seed(digest.finalize().into())
    }

    /// Ed25519 public key.
    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// The Stellar account whose master key is this signer.
    #[must_use]
    pub fn account_id(&self) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(self.public_key())))
    }

    /// The Soroban address of [`Self::account_id`].
    #[must_use]
    pub fn address(&self) -> ScAddress {
        ScAddress::Account(self.account_id())
    }

    /// The ledger key of [`Self::account_id`].
    #[must_use]
    pub fn account_key(&self) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: self.account_id(),
        })
    }

    fn sign(&self, payload: &[u8]) -> [u8; 64] {
        self.key.sign(payload).to_bytes()
    }
}

impl fmt::Debug for Ed25519Signer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Ed25519Signer")
            .field("account", &self.account_id().to_string())
            .finish_non_exhaustive()
    }
}

/// Credentials and signature payload of a signed authorization entry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum AuthorizationFormat {
    /// `SorobanCredentials::Address` over
    /// `HashIdPreimage::SorobanAuthorization`, as the Stellar SDKs sign and
    /// RPC simulation records by default.
    #[default]
    Address,
    /// `SorobanCredentials::AddressV2` over
    /// `HashIdPreimage::SorobanAuthorizationWithAddress`, which also binds
    /// the address. Stellar Core accepts it from Protocol 27.
    AddressV2,
}

impl AuthorizationTree {
    /// An authorization entry for this tree with
    /// `SorobanCredentials::SourceAccount`.
    ///
    /// The Host accepts it only when [`Self::address`] is the transaction
    /// source account: the transaction signature authorizes the tree, so the
    /// entry carries no signature or nonce.
    #[must_use]
    pub fn source_account_authorization(&self) -> SorobanAuthorizationEntry {
        SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: self.invocation.clone(),
        }
    }
}

/// Per-transaction Soroban limits of the captured network.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TransactionLimits {
    /// CPU instructions; also the most a transaction may declare.
    pub instructions: u64,
    /// Host memory bytes.
    pub memory_bytes: u64,
    /// Read-only plus read-write footprint keys.
    pub footprint_entries: u32,
    /// Classic footprint entries plus automatically restored entries.
    pub disk_read_entries: u32,
    /// Declared disk-read bytes.
    pub disk_read_bytes: u32,
    /// Read-write footprint keys.
    pub write_entries: u32,
    /// Declared write bytes.
    pub write_bytes: u32,
    /// Encoded contract events plus the encoded return value.
    pub contract_events_size_bytes: u32,
    /// Encoded transaction envelope.
    pub transaction_size_bytes: u32,
    /// Every encoded footprint key.
    pub ledger_key_size_bytes: u32,
    /// Encoded `ContractData` entries read or written.
    pub ledger_entry_size_bytes: u32,
    /// Encoded `ContractCode` entries read or written.
    pub contract_code_size_bytes: u32,
}

/// One limited transaction resource; each variant names the
/// [`TransactionLimits`] field of the same meaning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TransactionResource {
    Instructions,
    MemoryBytes,
    FootprintEntries,
    DiskReadEntries,
    DiskReadBytes,
    WriteEntries,
    WriteBytes,
    ContractEventsSizeBytes,
    TransactionSizeBytes,
    LedgerKeySizeBytes,
    LedgerEntrySizeBytes,
    ContractCodeSizeBytes,
}

/// A resource above its declared amount or network limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ResourceExcess {
    pub resource: TransactionResource,
    /// The declared amount or the network limit.
    pub limit: u64,
    pub used: u64,
}

/// Resource fee components in stroops, computed with the Host's fee
/// functions and the captured fee rates.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ResourceFee {
    /// Charged on the declared instructions, ledger entries, read/write
    /// bytes, and the transaction size.
    pub non_refundable: i64,
    /// Consumed contract events and rent; zero for a failed transaction.
    pub refundable: i64,
    /// Rent part of [`Self::refundable`].
    pub rent: i64,
}

impl ResourceFee {
    /// Non-refundable plus refundable fee.
    #[must_use]
    pub const fn total(&self) -> i64 {
        self.non_refundable.saturating_add(self.refundable)
    }
}

/// Measured resources of one simulated or applied transaction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ResourceReport {
    /// CPU instructions consumed by the Host.
    pub instructions: u64,
    /// Memory bytes consumed by the Host.
    pub memory_bytes: u64,
    /// Read-only plus read-write footprint keys.
    pub footprint_entries: u32,
    /// Classic footprint entries plus automatically restored entries.
    pub disk_read_entries: u32,
    /// Encoded size of the entries counted in [`Self::disk_read_entries`].
    pub disk_read_bytes: u32,
    /// Read-write footprint entries.
    pub write_entries: u32,
    /// Encoded size of the read-write entries after execution.
    pub write_bytes: u32,
    /// Encoded contract events plus the encoded return value.
    pub contract_events_size_bytes: u32,
    /// Encoded transaction envelope.
    pub transaction_size_bytes: u32,
    /// Fee components; zero for a simulation, whose declared total is
    /// `transaction_data.resource_fee`.
    pub fee: ResourceFee,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FootprintCounts {
    pub(crate) keys: u32,
    pub(crate) disk_reads: u32,
    pub(crate) writes: u32,
}

/// A Soroban host function as the single `InvokeHostFunctionOp` of a
/// transaction whose source account is controlled by a known key.
///
/// The envelope defaults to what the Stellar SDKs build: an inclusion fee
/// of 100 stroops, open time bounds, no memo, an unmuxed source, and one
/// signature. Only its size and validity matter to Kanatoko; signatures are
/// never verified, because the source key is known.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionCall {
    source: AccountId,
    function: HostFunction,
    inclusion_fee: u32,
    time_bounds: Option<TimeBounds>,
    memo: Memo,
    muxed_id: Option<u64>,
    signature_count: u8,
}

impl TransactionCall {
    /// A call of `function` from the account of `source`.
    ///
    /// The source is a signer rather than a bare account so that
    /// source-account authorization is always backed by a key the test
    /// holds.
    #[must_use]
    pub fn new(source: &Ed25519Signer, function: HostFunction) -> Self {
        Self {
            source: source.account_id(),
            function,
            inclusion_fee: DEFAULT_INCLUSION_FEE,
            time_bounds: Some(TimeBounds {
                min_time: TimePoint(0),
                max_time: TimePoint(0),
            }),
            memo: Memo::None,
            muxed_id: None,
            signature_count: 1,
        }
    }

    /// `HostFunction::InvokeContract` for a detached contract call.
    ///
    /// # Errors
    ///
    /// Returns `TransactionError::Strict(StrictForkError::InvalidInvocationXdr)`
    /// when the arguments exceed the XDR vector bound.
    pub fn invoke(
        source: &Ed25519Signer,
        request: &InvokeRequest,
    ) -> Result<Self, TransactionError> {
        Ok(Self::new(
            source,
            HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: request.contract.clone(),
                function_name: request.function.clone(),
                args: xdr_vec(request.args.clone())?,
            }),
        ))
    }

    /// `HostFunction::UploadContractWasm` of `wasm`.
    ///
    /// The fixture must hold the `ContractCode` key of this WASM, captured
    /// present or absent, for example with
    /// [`crate::ScenarioFork::capture_wasm`].
    ///
    /// # Errors
    ///
    /// Returns `TransactionError::Strict(StrictForkError::InvalidInvocationXdr)`
    /// when `wasm` exceeds the XDR byte bound.
    pub fn upload_wasm(source: &Ed25519Signer, wasm: &[u8]) -> Result<Self, TransactionError> {
        Ok(Self::new(
            source,
            HostFunction::UploadContractWasm(
                wasm.to_vec()
                    .try_into()
                    .map_err(|_| StrictForkError::InvalidInvocationXdr)?,
            ),
        ))
    }

    /// `HostFunction::CreateContractV2` of uploaded WASM `wasm_hash` at the
    /// address derived from `deployer` and `salt`, running its constructor
    /// with `constructor_args`.
    ///
    /// The deployer authorizes the creation: with source-account credentials
    /// when it is the transaction source, otherwise with a signed entry.
    /// [`StrictFork::contract_address`] derives the resulting address; the
    /// fixture must hold its contract instance key, for example with
    /// [`crate::ScenarioFork::capture_deployed_contract`].
    ///
    /// # Errors
    ///
    /// Returns `TransactionError::Strict(StrictForkError::InvalidInvocationXdr)`
    /// when the arguments exceed the XDR vector bound.
    pub fn create_contract(
        source: &Ed25519Signer,
        deployer: ScAddress,
        salt: [u8; 32],
        wasm_hash: [u8; 32],
        constructor_args: Vec<ScVal>,
    ) -> Result<Self, TransactionError> {
        Ok(Self::new(
            source,
            HostFunction::CreateContractV2(CreateContractArgsV2 {
                contract_id_preimage: ContractIdPreimage::Address(ContractIdPreimageFromAddress {
                    address: deployer,
                    salt: Uint256(salt),
                }),
                executable: ContractExecutable::Wasm(Hash(wasm_hash)),
                constructor_args: xdr_vec(constructor_args)?,
            }),
        ))
    }

    /// The same call bidding `inclusion_fee` stroops above the resource fee.
    /// Core charges at most the network base fee of it outside surge pricing
    /// and refuses bids below the base fee.
    #[must_use]
    pub const fn with_inclusion_fee(mut self, inclusion_fee: u32) -> Self {
        self.inclusion_fee = inclusion_fee;
        self
    }

    /// The same call with `time_bounds` as preconditions; `None` sends none.
    /// A zero bound is open. Apply checks them against the applying ledger's
    /// close time.
    #[must_use]
    pub const fn with_time_bounds(mut self, time_bounds: Option<TimeBounds>) -> Self {
        self.time_bounds = time_bounds;
        self
    }

    /// The same call with `memo`. Core refuses a Soroban transaction with a
    /// memo, so apply fails with [`InvalidTransaction::MemoOrMuxedSource`].
    #[must_use]
    pub fn with_memo(mut self, memo: Memo) -> Self {
        self.memo = memo;
        self
    }

    /// The same call from the source account multiplexed with `id`. Core
    /// refuses a Soroban transaction with a muxed source, so apply fails
    /// with [`InvalidTransaction::MemoOrMuxedSource`].
    #[must_use]
    pub const fn with_muxed_source(mut self, id: u64) -> Self {
        self.muxed_id = Some(id);
        self
    }

    /// The same call with `count` envelope signatures, which only changes the
    /// transaction size.
    ///
    /// # Panics
    ///
    /// Panics unless `count` is between 1 and 20.
    #[must_use]
    pub fn with_signature_count(mut self, count: u8) -> Self {
        assert!(
            (1..=MAX_SIGNATURES).contains(&count),
            "a transaction has 1 to 20 signatures"
        );
        self.signature_count = count;
        self
    }

    /// Transaction source account.
    #[must_use]
    pub const fn source(&self) -> &AccountId {
        &self.source
    }

    #[must_use]
    pub const fn function(&self) -> &HostFunction {
        &self.function
    }

    /// Inclusion fee bid in stroops.
    #[must_use]
    pub const fn inclusion_fee(&self) -> u32 {
        self.inclusion_fee
    }
}

/// Detached evidence of one simulated, prepared, or applied transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TransactionReceipt {
    pub call: TransactionCall,
    /// Authorization entries the Host recorded (simulation) or was given.
    pub authorization: Vec<SorobanAuthorizationEntry>,
    /// Declared footprint, resources, and resource fee; for a simulation, the
    /// values RPC simulation would return.
    pub transaction_data: SorobanTransactionData,
    pub outcome: InvokeOutcome,
    /// The operation result code Stellar Core would report.
    pub result: InvokeHostFunctionResultCode,
    /// The resource behind [`InvokeHostFunctionResultCode::ResourceLimitExceeded`]
    /// when Kanatoko detected it.
    pub exceeded: Option<ResourceExcess>,
    pub events: Vec<DetachedEvent>,
    pub diagnostics: Vec<DetachedEvent>,
    /// Ledger changes, including the source account's fee and sequence
    /// number for an applied transaction.
    pub state_changes: Vec<StateChange>,
    pub before_digest: [u8; 32],
    pub after_digest: [u8; 32],
    pub disposition: ReceiptDisposition,
    /// Ledger the transaction was simulated against or applied in.
    pub ledger_sequence: u32,
    /// Fee charged to the source account after the refund; zero for a
    /// simulation.
    pub fee_charged: i64,
    pub resources: ResourceReport,
}

/// A recording-mode simulation, as RPC `simulateTransaction` performs it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionSimulation {
    receipt: TransactionReceipt,
    authorization: Vec<AuthorizationTree>,
}

impl TransactionSimulation {
    /// Evidence of the simulation; its disposition is always
    /// [`ReceiptDisposition::Previewed`].
    #[must_use]
    pub const fn receipt(&self) -> &TransactionReceipt {
        &self.receipt
    }

    /// Authorization trees the call requires, in Host order. A tree whose
    /// address is the transaction source account was recorded for
    /// source-account credentials.
    #[must_use]
    pub fn authorization(&self) -> &[AuthorizationTree] {
        &self.authorization
    }
}

/// A transaction with authorization and declared resources, ready for
/// [`StrictFork::apply_transaction`].
///
/// The `with_*` and `without_*` methods derive modified copies for negative
/// tests; they never change resources to match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedTransaction {
    call: TransactionCall,
    authorization: Vec<SorobanAuthorizationEntry>,
    transaction_data: SorobanTransactionData,
}

impl PreparedTransaction {
    #[must_use]
    pub const fn call(&self) -> &TransactionCall {
        &self.call
    }

    #[must_use]
    pub fn authorization(&self) -> &[SorobanAuthorizationEntry] {
        &self.authorization
    }

    /// Declared footprint, resources, restored entries, and resource fee.
    #[must_use]
    pub const fn transaction_data(&self) -> &SorobanTransactionData {
        &self.transaction_data
    }

    /// Whether `key` is in the read-only or read-write footprint.
    #[must_use]
    pub fn footprint_contains(&self, key: &LedgerKey) -> bool {
        let footprint = &self.transaction_data.resources.footprint;
        footprint.read_only.contains(key) || footprint.read_write.contains(key)
    }

    /// The same transaction with another envelope shape, for example
    /// `call.clone().with_signature_count(3)`. The function must be
    /// unchanged.
    ///
    /// # Panics
    ///
    /// Panics if `call` has another source or function.
    #[must_use]
    pub fn with_call(mut self, call: TransactionCall) -> Self {
        assert!(
            call.source == self.call.source && call.function == self.call.function,
            "the envelope may change, the call may not"
        );
        self.call = call;
        self
    }

    /// The same transaction with `authorization` instead of its entries.
    #[must_use]
    pub fn with_authorization(mut self, authorization: Vec<SorobanAuthorizationEntry>) -> Self {
        self.authorization = authorization;
        self
    }

    /// The same transaction without authorization entry `index`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of range.
    #[must_use]
    pub fn without_authorization_entry(mut self, index: usize) -> Self {
        assert!(
            index < self.authorization.len(),
            "authorization entry index out of range"
        );
        self.authorization.remove(index);
        self
    }

    /// The same transaction with one bit of the first signature in
    /// authorization entry `index` flipped.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of range or the entry carries no signature
    /// bytes.
    #[must_use]
    pub fn with_tampered_signature(mut self, index: usize) -> Self {
        let entry = self
            .authorization
            .get_mut(index)
            .expect("authorization entry index out of range");
        let credentials = match &mut entry.credentials {
            SorobanCredentials::Address(credentials)
            | SorobanCredentials::AddressV2(credentials) => credentials,
            SorobanCredentials::AddressWithDelegates(credentials) => {
                &mut credentials.address_credentials
            }
            SorobanCredentials::SourceAccount => {
                panic!("source-account credentials carry no signature")
            }
        };
        assert!(
            tamper_signature(&mut credentials.signature),
            "authorization entry carries no signature bytes"
        );
        self
    }

    /// The same transaction with `transaction_data` as declared footprint,
    /// resources, restored entries, and resource fee. Apply validates it as
    /// Core does.
    #[must_use]
    pub fn with_transaction_data(mut self, transaction_data: SorobanTransactionData) -> Self {
        self.transaction_data = transaction_data;
        self
    }

    /// The same transaction without `key` in its footprint. Restored-entry
    /// indices are kept consistent with the shortened read-write footprint.
    ///
    /// # Panics
    ///
    /// Panics if `key` is not in the footprint.
    #[must_use]
    pub fn without_footprint_key(mut self, key: &LedgerKey) -> Self {
        let footprint = &mut self.transaction_data.resources.footprint;
        let mut read_only = footprint.read_only.to_vec();
        let mut read_write = footprint.read_write.to_vec();
        if let Some(index) = read_only.iter().position(|candidate| candidate == key) {
            read_only.remove(index);
        } else if let Some(index) = read_write.iter().position(|candidate| candidate == key) {
            read_write.remove(index);
            if let SorobanTransactionDataExt::V1(ext) = &mut self.transaction_data.ext {
                let index = u32::try_from(index).expect("footprint index fits u32");
                let restored = ext
                    .archived_soroban_entries
                    .iter()
                    .filter(|restored| **restored != index)
                    .map(|restored| {
                        if *restored > index {
                            restored - 1
                        } else {
                            *restored
                        }
                    })
                    .collect::<Vec<_>>();
                ext.archived_soroban_entries = restored
                    .try_into()
                    .expect("a shorter restored-entry list fits its bound");
            }
        } else {
            panic!("ledger key is not in the footprint");
        }
        *footprint = LedgerFootprint {
            read_only: read_only
                .try_into()
                .expect("a shorter footprint fits its bound"),
            read_write: read_write
                .try_into()
                .expect("a shorter footprint fits its bound"),
        };
        self
    }
}

/// Why Stellar Core would refuse a transaction before including it in a
/// ledger. Nothing is charged and the fork is unchanged.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum InvalidTransaction {
    #[error("{resource:?} of {used} exceeds the network per-transaction limit {limit}")]
    NetworkLimitExceeded {
        resource: TransactionResource,
        limit: u64,
        used: u64,
    },
    #[error("the footprint contains an unsupported ledger key")]
    UnsupportedFootprintKey(Box<LedgerKey>),
    #[error("the footprint contains a key twice")]
    DuplicateFootprintKey(Box<LedgerKey>),
    #[error("archived-entry indices are unsorted, out of range, or not persistent")]
    InvalidArchivedEntries,
    #[error("Soroban transactions may not have a memo or a muxed source")]
    MemoOrMuxedSource,
    #[error("the transaction fee overflows")]
    FeeOverflow,
    #[error("declared resource fee {declared} is below the required {required} stroops")]
    InsufficientResourceFee { declared: i64, required: i64 },
    #[error("inclusion fee {bid} is below the network base fee {minimum}")]
    InsufficientInclusionFee { bid: i64, minimum: i64 },
    #[error("the ledger closes at {close_time}, before the minimum time {min_time}")]
    TooEarly { min_time: u64, close_time: u64 },
    #[error("the ledger closes at {close_time}, after the maximum time {max_time}")]
    TooLate { max_time: u64, close_time: u64 },
    #[error("the transaction source account does not exist")]
    SourceAccountMissing,
    #[error("the source key does not meet its account's medium threshold")]
    SourceAccountNotAuthorized,
    #[error("available balance {available} is below the transaction fee {required}")]
    InsufficientBalance { available: i64, required: i64 },
    #[error("the transaction accesses a frozen ledger key")]
    FrozenKeyAccessed(Box<LedgerKey>),
}

/// How a transaction expected to fail failed.
#[derive(Debug)]
#[non_exhaustive]
#[must_use = "assert the reason of the failure"]
pub enum FailedApply {
    /// Included and failed: the fee was charged and the ledger closed, the
    /// invocation's changes were discarded.
    Rejected(Box<TransactionReceipt>),
    /// Refused before inclusion; nothing changed.
    Invalid(InvalidTransaction),
}

/// Failures of transaction-faithful execution.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransactionError {
    #[error(transparent)]
    Strict(#[from] StrictForkError),
    #[error(
        "the fixture lacks network ConfigSetting entries {missing:?}; capture it with \
         capture_network_config()"
    )]
    MissingNetworkConfig {
        missing: Vec<soroban_env_host::xdr::ConfigSettingId>,
    },
    #[error("the captured network configuration is not usable by this Host")]
    InvalidNetworkConfig,
    /// The network would refuse the transaction; the fork is unchanged.
    #[error(transparent)]
    Invalid(#[from] InvalidTransaction),
    /// The network would include the transaction and fail it: the fee was
    /// charged, the ledger closed, and the invocation's changes discarded.
    #[error("the transaction failed with {:?}", .0.result)]
    Rejected(Box<TransactionReceipt>),
    #[error("no signer was supplied for an authorization address")]
    MissingSigner { address: ScAddress },
    #[error("authorization nonce overflowed")]
    NonceOverflow,
    #[error("the transaction simulation failed")]
    SimulationFailed(Box<TransactionReceipt>),
    #[error("the transaction expected to fail succeeded and was not committed")]
    UnexpectedSuccess(Box<TransactionReceipt>),
    #[error("a failed transaction changed the fork beyond its fee and sequence number")]
    NotAtomic,
    #[error("the local account already exists on the network or the fork")]
    LocalAccountExists,
    #[error("a new local account needs at least {minimum} stroops")]
    LocalAccountBalanceTooLow { minimum: i64 },
}

impl TransactionError {
    /// Whether the network itself would fail this transaction:
    /// [`Self::Invalid`] or [`Self::Rejected`], rather than a fixture,
    /// configuration, or usage error.
    #[must_use]
    pub const fn is_network_failure(&self) -> bool {
        matches!(self, Self::Invalid(_) | Self::Rejected(_))
    }
}

impl From<crate::FixtureError> for TransactionError {
    fn from(error: crate::FixtureError) -> Self {
        Self::Strict(StrictForkError::Fixture(error))
    }
}

/// One included transaction before the fork commits it.
struct Executed {
    receipt: TransactionReceipt,
    after: LedgerSnapshot,
}

/// Why an included transaction failed.
struct Failure {
    outcome: InvokeOutcome,
    result: InvokeHostFunctionResultCode,
    exceeded: Option<ResourceExcess>,
}

impl Failure {
    fn limit(excess: ResourceExcess) -> Self {
        Self {
            outcome: InvokeOutcome::Failure {
                error: None,
                kind: InvokeErrorKind::Abort,
            },
            result: InvokeHostFunctionResultCode::ResourceLimitExceeded,
            exceeded: Some(excess),
        }
    }

    fn code(result: InvokeHostFunctionResultCode) -> Self {
        Self {
            outcome: InvokeOutcome::Failure {
                error: None,
                kind: InvokeErrorKind::Abort,
            },
            result,
            exceeded: None,
        }
    }
}

/// Footprint entries in Host order, disk-read bytes, and the WASM to cache.
struct FootprintRead {
    entries: Vec<(Option<Vec<u8>>, Option<TtlLedgerEntryMeta>)>,
    disk_read_bytes: u32,
    cached_code: Vec<Vec<u8>>,
}

impl StrictFork {
    /// Network ID of the captured network.
    #[must_use]
    pub fn network_id(&self) -> [u8; 32] {
        self.ledger_info().network_id
    }

    /// Sequence of the fork's last closed ledger. Simulation runs against it;
    /// [`Self::apply_transaction`] applies in the next one.
    #[must_use]
    pub fn ledger_sequence(&self) -> u32 {
        self.ledger_info().sequence_number
    }

    /// Close time of the fork's last closed ledger.
    #[must_use]
    pub fn ledger_close_time(&self) -> u64 {
        self.ledger_info().timestamp
    }

    /// Network configuration from the fixture's captured `ConfigSetting`
    /// entries.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::MissingNetworkConfig`] naming every
    /// required setting the fixture lacks.
    pub fn network_config(&self) -> Result<NetworkConfig, TransactionError> {
        NetworkConfig::from_lookup(|key| {
            let id = key_id(key).ok()?;
            match self.coverage.get(&id) {
                Some(LookupState::Present(entry, _)) => Some(entry.clone()),
                _ => None,
            }
        })
    }

    /// The current value of a ledger key.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::UnknownLedgerKeys`] when the key is neither
    /// captured nor owned by local setup.
    pub fn ledger_entry(&self, key: &LedgerKey) -> Result<Option<LedgerValue>, StrictForkError> {
        let id = key_id(key)?;
        match self.coverage.get(&id) {
            Some(LookupState::Present(entry, live_until)) => Ok(Some(LedgerValue {
                entry: (**entry).clone(),
                live_until_ledger: *live_until,
            })),
            Some(LookupState::Absent) => Ok(None),
            None if is_local_key(key, &self.local_contracts, &self.local_code_hashes, false) => {
                Ok(None)
            }
            None => Err(unknown(vec![id])),
        }
    }

    /// The contract address `CreateContractV2` derives from `deployer` and
    /// `salt` on the captured network.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::Xdr`] if the preimage cannot be encoded.
    pub fn contract_address(
        &self,
        deployer: &ScAddress,
        salt: [u8; 32],
    ) -> Result<ScAddress, StrictForkError> {
        contract_address(self.network_id(), deployer, salt)
    }

    /// Explicitly injects the account of `signer` with `balance` stroops,
    /// master weight 1, and zero thresholds.
    ///
    /// The account key must be captured as absent, for example with
    /// [`crate::ScenarioFork::capture_signer`], so a test never shadows a
    /// real account. This is local ledger injection for test identities, not
    /// a `CreateAccount` operation. The new account's trustlines and
    /// authorization nonces are local knowledge from then on.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::LocalAccountExists`] when the account
    /// exists, [`StrictForkError::UnknownLedgerKeys`] when its key was not
    /// captured, and [`TransactionError::LocalAccountBalanceTooLow`] below
    /// two base reserves.
    pub fn create_local_account(
        &mut self,
        signer: &Ed25519Signer,
        balance: i64,
    ) -> Result<StateChange, TransactionError> {
        let key = signer.account_key();
        let id = key_id(&key)?;
        match self.coverage.get(&id) {
            Some(LookupState::Absent) => {}
            Some(LookupState::Present(_, _)) => return Err(TransactionError::LocalAccountExists),
            None => return Err(unknown(vec![id]).into()),
        }
        let ledger = self.ledger_info();
        let minimum = i64::from(ledger.base_reserve).saturating_mul(2);
        if balance < minimum {
            return Err(TransactionError::LocalAccountBalanceTooLow { minimum });
        }
        let entry = LedgerEntry {
            last_modified_ledger_seq: ledger.sequence_number,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: signer.account_id(),
                balance,
                seq_num: SequenceNumber(i64::from(ledger.sequence_number) << 32),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        let address = signer.address();
        let newly_local = self.local_contracts.insert(address.clone());
        let change = self.override_entry(key, Some((entry, None)));
        if change.is_err() && newly_local {
            self.local_contracts.remove(&address);
        }
        Ok(change?)
    }

    /// Explicitly writes or removes one contract data entry.
    ///
    /// This is a local state override for test setup, not contract
    /// execution: no contract code, authorization, or event runs. An existing
    /// entry keeps its live-until ledger; a new one lives for the network's
    /// minimum TTL of its durability.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::UnknownLedgerKeys`] when the key is neither
    /// captured nor owned by local setup.
    pub fn set_contract_data(
        &mut self,
        contract: &ScAddress,
        key: ScVal,
        durability: ContractDataDurability,
        value: Option<ScVal>,
    ) -> Result<StateChange, StrictForkError> {
        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract.clone(),
            key: key.clone(),
            durability,
        });
        let existing = self.ledger_entry(&ledger_key)?;
        let Some(val) = value else {
            return self.override_entry(ledger_key, None);
        };
        let ledger = self.ledger_info();
        let live_until = existing
            .as_ref()
            .and_then(|existing| existing.live_until_ledger)
            .unwrap_or_else(|| {
                let ttl = match durability {
                    ContractDataDurability::Persistent => ledger.min_persistent_entry_ttl,
                    ContractDataDurability::Temporary => ledger.min_temp_entry_ttl,
                };
                ledger.sequence_number.saturating_add(ttl.saturating_sub(1))
            });
        let entry = LedgerEntry {
            last_modified_ledger_seq: ledger.sequence_number,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: contract.clone(),
                key,
                durability,
                val,
            }),
            ext: LedgerEntryExt::V0,
        };
        self.override_entry(ledger_key, Some((entry, Some(live_until))))
    }

    /// [`Self::invoke`] with `source` as the Host's transaction source
    /// account.
    ///
    /// Under [`crate::AuthMode::Enforce`], an entry with
    /// `SorobanCredentials::SourceAccount`, for example from
    /// [`AuthorizationTree::source_account_authorization`], then authorizes
    /// the source account. The source must be a signer so that this
    /// authorization is always backed by a key the test holds.
    ///
    /// # Errors
    ///
    /// The same as [`Self::invoke`].
    pub fn invoke_with_source(
        &mut self,
        source: &Ed25519Signer,
        request: InvokeRequest,
        execution: crate::ExecutionMode,
        auth: crate::AuthMode,
    ) -> Result<Receipt, StrictForkError> {
        self.invoke_from(Some(source.account_id()), request, execution, auth)
    }

    /// Signs `tree` for its address with `signer` in
    /// [`AuthorizationFormat::Address`], as the Stellar SDKs do.
    ///
    /// The Host verifies that `signer` is a signer of the tree's account
    /// with enough weight, that `nonce` is unused, and that
    /// `signature_expiration_ledger` is neither past nor beyond the maximum
    /// entry TTL of the applying ledger.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::Xdr`] if the payload cannot be encoded.
    pub fn sign_authorization(
        &self,
        tree: &AuthorizationTree,
        signer: &Ed25519Signer,
        nonce: i64,
        signature_expiration_ledger: u32,
    ) -> Result<SorobanAuthorizationEntry, TransactionError> {
        self.sign_authorization_as(
            AuthorizationFormat::Address,
            tree,
            signer,
            nonce,
            signature_expiration_ledger,
        )
    }

    /// [`Self::sign_authorization`] in `format`. The signature value has the
    /// Stellar account contract's shape: a vector holding one
    /// `{public_key, signature}` map.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::Xdr`] if the payload cannot be encoded.
    pub fn sign_authorization_as(
        &self,
        format: AuthorizationFormat,
        tree: &AuthorizationTree,
        signer: &Ed25519Signer,
        nonce: i64,
        signature_expiration_ledger: u32,
    ) -> Result<SorobanAuthorizationEntry, TransactionError> {
        let network_id = Hash(self.network_id());
        let preimage = match format {
            AuthorizationFormat::Address => {
                HashIdPreimage::SorobanAuthorization(HashIdPreimageSorobanAuthorization {
                    network_id,
                    nonce,
                    signature_expiration_ledger,
                    invocation: tree.invocation.clone(),
                })
            }
            AuthorizationFormat::AddressV2 => HashIdPreimage::SorobanAuthorizationWithAddress(
                HashIdPreimageSorobanAuthorizationWithAddress {
                    network_id,
                    nonce,
                    signature_expiration_ledger,
                    address: tree.address.clone(),
                    invocation: tree.invocation.clone(),
                },
            ),
        };
        let payload = Sha256::digest(encode(&preimage)?);
        let credentials = SorobanAddressCredentials {
            address: tree.address.clone(),
            nonce,
            signature_expiration_ledger,
            signature: account_signature(signer.public_key(), signer.sign(&payload))?,
        };
        Ok(SorobanAuthorizationEntry {
            credentials: match format {
                AuthorizationFormat::Address => SorobanCredentials::Address(credentials),
                AuthorizationFormat::AddressV2 => SorobanCredentials::AddressV2(credentials),
            },
            root_invocation: tree.invocation.clone(),
        })
    }

    /// Authorizes every tree of `simulation` in
    /// [`AuthorizationFormat::Address`]: source-account credentials for the
    /// transaction source and an entry signed by the matching signer for
    /// every other account. Tree `i` in Host order uses nonce
    /// `first_nonce + i`.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::MissingSigner`] when no signer's account
    /// is a tree's address, for example a contract address.
    pub fn authorize(
        &self,
        simulation: &TransactionSimulation,
        signers: &[&Ed25519Signer],
        first_nonce: i64,
        signature_expiration_ledger: u32,
    ) -> Result<Vec<SorobanAuthorizationEntry>, TransactionError> {
        self.authorize_as(
            AuthorizationFormat::Address,
            simulation,
            signers,
            first_nonce,
            signature_expiration_ledger,
        )
    }

    /// [`Self::authorize`] with signed entries in `format`.
    ///
    /// # Errors
    ///
    /// The same as [`Self::authorize`].
    pub fn authorize_as(
        &self,
        format: AuthorizationFormat,
        simulation: &TransactionSimulation,
        signers: &[&Ed25519Signer],
        first_nonce: i64,
        signature_expiration_ledger: u32,
    ) -> Result<Vec<SorobanAuthorizationEntry>, TransactionError> {
        let source = ScAddress::Account(simulation.receipt.call.source.clone());
        let mut entries = Vec::with_capacity(simulation.authorization.len());
        for (index, tree) in simulation.authorization.iter().enumerate() {
            if tree.address == source {
                entries.push(tree.source_account_authorization());
                continue;
            }
            let signer = signers
                .iter()
                .find(|signer| signer.address() == tree.address)
                .ok_or_else(|| TransactionError::MissingSigner {
                    address: tree.address.clone(),
                })?;
            let nonce = i64::try_from(index)
                .ok()
                .and_then(|index| first_nonce.checked_add(index))
                .ok_or(TransactionError::NonceOverflow)?;
            entries.push(self.sign_authorization_as(
                format,
                tree,
                signer,
                nonce,
                signature_expiration_ledger,
            )?);
        }
        Ok(entries)
    }

    /// Simulates `call` with recorded authorization, as RPC
    /// `simulateTransaction` does, against the fork's last closed ledger,
    /// and returns the authorization trees to sign. The fork never changes.
    ///
    /// The receipt's state changes report entries; their live-until ledgers
    /// are the previous ones, or the minimum TTL for new entries.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::MissingNetworkConfig`] without captured
    /// network configuration and [`StrictForkError::UnknownLedgerKeys`] when
    /// the call touches a key outside the fixture.
    pub fn simulate_transaction(
        &self,
        call: &TransactionCall,
    ) -> Result<TransactionSimulation, TransactionError> {
        let receipt = self.simulate(call, None)?;
        let authorization = receipt
            .authorization
            .iter()
            .map(|entry| AuthorizationTree {
                address: credentials_address(&entry.credentials, &call.source),
                invocation: entry.root_invocation.clone(),
            })
            .collect();
        Ok(TransactionSimulation {
            receipt,
            authorization,
        })
    }

    /// Simulates `call` with the supplied, enforced `authorization`, as RPC
    /// `simulateTransaction` does for signed entries, and returns the
    /// transaction with RPC's footprint, adjusted resources, and resource
    /// fee. The fork never changes.
    ///
    /// The adjustments are those of RPC simulation:
    /// `SimulationAdjustmentConfig::default_adjustment()` of
    /// `soroban-simulation`, which declares instructions as
    /// `max(1.04 × measured, measured + 50,000)`, read and write bytes as
    /// measured, a resource fee for the largest envelope the operation can
    /// have plus 10% (at least 500 bytes), and the refundable fee plus 15%.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::SimulationFailed`] with the evidence when
    /// the call fails, for example for an invalid signature, plus the errors
    /// of [`Self::simulate_transaction`].
    pub fn prepare_transaction(
        &self,
        call: &TransactionCall,
        authorization: Vec<SorobanAuthorizationEntry>,
    ) -> Result<PreparedTransaction, TransactionError> {
        let receipt = self.simulate(call, Some(authorization))?;
        if !receipt.outcome.is_success() {
            return Err(TransactionError::SimulationFailed(Box::new(receipt)));
        }
        Ok(PreparedTransaction {
            call: call.clone(),
            authorization: receipt.authorization,
            transaction_data: receipt.transaction_data,
        })
    }

    /// Simulates `call`, authorizes it with [`Self::authorize`], and prepares
    /// it: the usual path from a call to a transaction ready for
    /// [`Self::apply_transaction`].
    ///
    /// # Errors
    ///
    /// The errors of [`Self::simulate_transaction`], [`Self::authorize`], and
    /// [`Self::prepare_transaction`], and
    /// [`TransactionError::SimulationFailed`] when the recording run fails.
    pub fn sign_transaction(
        &self,
        call: &TransactionCall,
        signers: &[&Ed25519Signer],
        first_nonce: i64,
        signature_expiration_ledger: u32,
    ) -> Result<PreparedTransaction, TransactionError> {
        let simulation = self.simulate_transaction(call)?;
        if !simulation.receipt.outcome.is_success() {
            return Err(TransactionError::SimulationFailed(Box::new(
                simulation.receipt,
            )));
        }
        let authorization = self.authorize(
            &simulation,
            signers,
            first_nonce,
            signature_expiration_ledger,
        )?;
        self.prepare_transaction(call, authorization)
    }

    /// Applies `transaction` as Stellar Core applies a transaction with one
    /// `InvokeHostFunctionOp`, in the next ledger.
    ///
    /// Validation first refuses what Core would not include, with
    /// [`TransactionError::Invalid`] and no change: limits of the declared
    /// resources, footprint, key sizes, and envelope size; footprint key
    /// types, duplicates, and archived-entry indices; a memo or muxed source;
    /// a resource fee below the non-refundable fee or an inclusion fee below
    /// the base fee; time bounds against the next close time; a missing
    /// source, a source key below its medium threshold, an available balance
    /// below the full fee, and frozen keys.
    ///
    /// An included transaction closes ledger `N + 1` at the previous close
    /// time plus the network's target close time. The source pays the
    /// resource fee plus the inclusion fee, capped at the base fee, and its
    /// sequence number advances, as Core processes fees and sequence numbers
    /// before applying. The Host then executes with the enforcing footprint,
    /// the declared instructions and the network memory limit as its budget,
    /// the supplied authorization entries, and a module cache of the
    /// footprint's WASM, as Core caches deployed code. Archived entries not
    /// marked for restore, read entries above the entry size limits, more
    /// disk-read or written bytes than declared, events above the limit, and
    /// a refundable fee that does not cover events and rent fail the
    /// transaction, as Core fails it. The unused refundable fee is refunded;
    /// a failed transaction gets its whole refundable fee back.
    ///
    /// Envelope signatures are not verified: [`TransactionCall::new`] takes
    /// the source's key.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::Invalid`] for a refused transaction and
    /// [`TransactionError::Rejected`] for a failed one, after charging its
    /// fee and closing the ledger. Missing network configuration or an
    /// uncaptured footprint key fail closed without any change.
    pub fn apply_transaction(
        &mut self,
        transaction: &PreparedTransaction,
    ) -> Result<TransactionReceipt, TransactionError> {
        let executed = self.execute(transaction)?;
        self.commit(&executed.after, &executed.receipt.state_changes)?;
        match executed.receipt.disposition {
            ReceiptDisposition::Committed => Ok(executed.receipt),
            _ => Err(TransactionError::Rejected(Box::new(executed.receipt))),
        }
    }

    /// Applies a `transaction` that must fail and proves that the failure
    /// was atomic: a refused transaction left the ledger digest unchanged,
    /// and a failed one changed only the source account's balance by the fee
    /// charged and its sequence number, in one closed ledger.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::UnexpectedSuccess`] if the transaction
    /// succeeds; it is not committed. Returns [`TransactionError::NotAtomic`]
    /// if the proof fails, and every error that is not a network failure,
    /// such as missing network configuration or an uncaptured key.
    pub fn expect_failed_apply(
        &mut self,
        transaction: &PreparedTransaction,
    ) -> Result<FailedApply, TransactionError> {
        let before = self.ledger_snapshot();
        let before_digest = canonical_ledger_digest(&before)?;
        match self.execute(transaction) {
            Err(TransactionError::Invalid(reason)) => {
                if self.ledger_digest()? != before_digest {
                    return Err(TransactionError::NotAtomic);
                }
                Ok(FailedApply::Invalid(reason))
            }
            Err(error) => Err(error),
            Ok(executed) if executed.receipt.disposition == ReceiptDisposition::Committed => {
                let mut receipt = executed.receipt;
                receipt.disposition = ReceiptDisposition::Previewed;
                Err(TransactionError::UnexpectedSuccess(Box::new(receipt)))
            }
            Ok(executed) => {
                self.commit(&executed.after, &executed.receipt.state_changes)?;
                let after = self.ledger_snapshot();
                prove_fee_only_change(
                    &before,
                    &after,
                    &transaction.call.source,
                    &executed.receipt,
                )?;
                Ok(FailedApply::Rejected(Box::new(executed.receipt)))
            }
        }
    }

    fn ledger_info(&self) -> LedgerInfo {
        // The fork Env always carries ledger info; its protocol is the
        // executed protocol.
        self.env
            .host()
            .with_ledger_info(|ledger| Ok(ledger.clone()))
            .unwrap_or_default()
    }

    fn override_entry(
        &mut self,
        key: LedgerKey,
        value: Option<(LedgerEntry, Option<u32>)>,
    ) -> Result<StateChange, StrictForkError> {
        let before = self.ledger_snapshot();
        let mut after = before.clone();
        set_entry(&mut after, &key, value)?;
        let changes = state_diff(&before, &after)?;
        self.commit(&after, &changes)?;
        Ok(changes.into_iter().next().unwrap_or(StateChange {
            key,
            before: None,
            after: None,
        }))
    }

    fn commit(
        &mut self,
        after: &LedgerSnapshot,
        changes: &[StateChange],
    ) -> Result<(), StrictForkError> {
        let mut coverage = self.coverage.clone();
        update_coverage(&mut coverage, changes)?;
        // A contract created after its instance key was captured absent has
        // no other ledger data, so all of its keys are local knowledge.
        for change in changes {
            if let LedgerKey::ContractData(data) = &change.key {
                if data.key == ScVal::LedgerKeyContractInstance
                    && change.before.is_none()
                    && change.after.is_some()
                {
                    self.local_contracts.insert(data.contract.clone());
                }
            }
        }
        let (env, _) = strict_env(
            after,
            &coverage,
            &self.local_contracts,
            &self.local_code_hashes,
            false,
        );
        self.coverage = coverage;
        self.env = env;
        Ok(())
    }

    fn source(&self, allow_mock_nonces: bool) -> Rc<StrictSource> {
        Rc::new(StrictSource::new(
            &self.coverage,
            &self.known_contracts(),
            &self.local_code_hashes,
            allow_mock_nonces,
        ))
    }

    /// Local contracts plus every contract whose instance key was captured
    /// absent. Contract data cannot exist without its contract having been
    /// created, and RPC reports archived instances too, so the data keys of
    /// such a contract are known to be absent; this is what lets a
    /// transaction create a contract at a captured-absent address.
    fn known_contracts(&self) -> std::collections::BTreeSet<ScAddress> {
        let mut contracts = self.local_contracts.clone();
        for (id, state) in &self.coverage {
            if !matches!(state, LookupState::Absent) {
                continue;
            }
            if let Ok(LedgerKey::ContractData(data)) = LedgerKey::from_xdr(id, Limits::none()) {
                if data.key == ScVal::LedgerKeyContractInstance {
                    contracts.insert(data.contract);
                }
            }
        }
        contracts
    }

    fn prng_seed(&self, call: &TransactionCall) -> Result<[u8; 32], TransactionError> {
        let mut digest = Sha256::new();
        digest.update(PRNG_SEED_DOMAIN);
        digest.update(self.network_id());
        digest.update(self.ledger_sequence().to_be_bytes());
        digest.update(encode(&call.source)?);
        digest.update(encode(&call.function)?);
        Ok(digest.finalize().into())
    }

    /// RPC simulation: `authorization` `None` records authorization (legacy
    /// `Address` credentials and root-only auth, RPC's defaults), otherwise
    /// the entries are enforced.
    #[allow(clippy::too_many_lines)]
    fn simulate(
        &self,
        call: &TransactionCall,
        authorization: Option<Vec<SorobanAuthorizationEntry>>,
    ) -> Result<TransactionReceipt, TransactionError> {
        self.network_config()?;
        let before = self.ledger_snapshot();
        let before_digest = canonical_ledger_digest(&before)?;
        let ledger = self.ledger_info();
        let source = self.source(authorization.is_none());
        let preflight = PreflightConfig::load_from_snapshot(source.as_ref())
            .map_err(|_| TransactionError::InvalidNetworkConfig)?;
        let mode = match authorization {
            None => RecordingInvocationAuthMode::recording(true, false),
            Some(entries) => RecordingInvocationAuthMode::Enforcing(entries),
        };
        let simulated = simulate_invoke_host_function_op(
            source.clone(),
            &preflight,
            &SimulationAdjustmentConfig::default_adjustment(),
            &ledger,
            call.function.clone(),
            mode,
            &call.source,
            self.prng_seed(call)?,
            true,
        )
        .map_err(|_| TransactionError::InvalidNetworkConfig)?;
        let unknown_keys = source.unknown.borrow().clone();
        if !unknown_keys.is_empty() {
            return Err(unknown(unknown_keys.into_iter().collect()).into());
        }

        let transaction_data = simulated
            .transaction_data
            .clone()
            .unwrap_or_else(empty_transaction_data);
        let (outcome, result) = match &simulated.invoke_result {
            Ok(value) => (
                InvokeOutcome::Success(value.clone()),
                InvokeHostFunctionResultCode::Success,
            ),
            Err(error) => (
                failure_outcome(error),
                InvokeHostFunctionResultCode::Trapped,
            ),
        };
        let mut after = before.clone();
        for diff in &simulated.modified_entries {
            let Some(key) = diff
                .state_after
                .as_ref()
                .or(diff.state_before.as_ref())
                .map(LedgerEntry::to_key)
            else {
                continue;
            };
            let previous = self.ledger_entry(&key).ok().flatten();
            let value = diff.state_after.clone().map(|entry| {
                let live_until = if is_soroban_key(&key) {
                    previous
                        .and_then(|previous| previous.live_until_ledger)
                        .or_else(|| new_entry_live_until(&ledger, &key))
                } else {
                    None
                };
                (entry, live_until)
            });
            set_entry(&mut after, &key, value)?;
        }
        let state_changes = state_diff(&before, &after)?;
        let events_size = encoded_events_size(
            simulated.contract_events.iter(),
            simulated.invoke_result.as_ref().ok(),
        )?;
        let counts = footprint_counts(
            &transaction_data.resources.footprint,
            restored_indices(&transaction_data),
        );
        let transaction_size_bytes = transaction_size(call, &simulated.auth, &transaction_data)?;
        Ok(TransactionReceipt {
            call: call.clone(),
            authorization: simulated.auth,
            outcome,
            result,
            exceeded: None,
            events: simulated
                .contract_events
                .into_iter()
                .map(|event| DetachedEvent {
                    event,
                    failed_call: false,
                })
                .collect(),
            diagnostics: detached_diagnostics(simulated.diagnostic_events),
            state_changes,
            before_digest,
            after_digest: canonical_ledger_digest(&after)?,
            disposition: ReceiptDisposition::Previewed,
            ledger_sequence: ledger.sequence_number,
            fee_charged: 0,
            resources: ResourceReport {
                instructions: u64::from(simulated.simulated_instructions),
                memory_bytes: u64::from(simulated.simulated_memory),
                footprint_entries: counts.keys,
                disk_read_entries: counts.disk_reads,
                disk_read_bytes: transaction_data.resources.disk_read_bytes,
                write_entries: counts.writes,
                write_bytes: transaction_data.resources.write_bytes,
                contract_events_size_bytes: events_size,
                transaction_size_bytes,
                fee: ResourceFee::default(),
            },
            transaction_data,
        })
    }

    /// Core's validation of `transaction`; on success, the source account
    /// entry and its available balance.
    fn validate(
        &self,
        config: &NetworkConfig,
        transaction: &PreparedTransaction,
        transaction_size_bytes: u32,
        next: &LedgerInfo,
    ) -> Result<(AccountEntry, i64), TransactionError> {
        let call = &transaction.call;
        let data = &transaction.transaction_data;
        if call.memo != Memo::None || call.muxed_id.is_some() {
            return Err(InvalidTransaction::MemoOrMuxedSource.into());
        }
        check_soroban_resources(&config.limits(), data, transaction_size_bytes)?;
        let full_fee = i64::from(call.inclusion_fee).saturating_add(data.resource_fee);
        if data.resource_fee < 0 || u32::try_from(full_fee).is_err() {
            return Err(InvalidTransaction::FeeOverflow.into());
        }
        let counts = footprint_counts(&data.resources.footprint, restored_indices(data));
        let (non_refundable, _) =
            config.resource_fees(&data.resources, counts, transaction_size_bytes, 0);
        if data.resource_fee < non_refundable {
            return Err(InvalidTransaction::InsufficientResourceFee {
                declared: data.resource_fee,
                required: non_refundable,
            }
            .into());
        }
        check_time_bounds(call.time_bounds.as_ref(), next.timestamp)?;
        if i64::from(call.inclusion_fee) < BASE_FEE {
            return Err(InvalidTransaction::InsufficientInclusionFee {
                bid: i64::from(call.inclusion_fee),
                minimum: BASE_FEE,
            }
            .into());
        }
        let source_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: call.source.clone(),
        });
        let account = match self.coverage.get(&key_id(&source_key)?) {
            Some(LookupState::Present(entry, _)) => match &entry.data {
                LedgerEntryData::Account(account) => account.clone(),
                _ => return Err(InvalidTransaction::SourceAccountMissing.into()),
            },
            Some(LookupState::Absent) => {
                return Err(InvalidTransaction::SourceAccountMissing.into())
            }
            None => return Err(unknown(vec![key_id(&source_key)?]).into()),
        };
        let [master, _, medium, _] = account.thresholds.0;
        if master == 0 || master < medium {
            return Err(InvalidTransaction::SourceAccountNotAuthorized.into());
        }
        let available = available_balance(&account, next.base_reserve);
        if available < full_fee {
            return Err(InvalidTransaction::InsufficientBalance {
                available,
                required: full_fee,
            }
            .into());
        }
        let footprint = &data.resources.footprint;
        for key in std::iter::once(&source_key)
            .chain(footprint.read_only.iter())
            .chain(footprint.read_write.iter())
        {
            if config.is_frozen(key) {
                return Err(InvalidTransaction::FrozenKeyAccessed(Box::new(key.clone())).into());
            }
        }
        Ok((account, available))
    }

    #[allow(clippy::too_many_lines)]
    fn execute(&self, transaction: &PreparedTransaction) -> Result<Executed, TransactionError> {
        let config = self.network_config()?;
        let limits = config.limits();
        let call = &transaction.call;
        let data = &transaction.transaction_data;
        let resources = &data.resources;
        let footprint = &resources.footprint;
        let restored = restored_indices(data);
        let transaction_size_bytes = transaction_size(call, &transaction.authorization, data)?;
        let ledger = self.ledger_info();
        let next = LedgerInfo {
            sequence_number: ledger.sequence_number.saturating_add(1),
            timestamp: ledger
                .timestamp
                .saturating_add(config.ledger_close_seconds()),
            ..ledger
        };
        let (account, _) = self.validate(&config, transaction, transaction_size_bytes, &next)?;

        // Every footprint key must be captured or local before anything runs.
        let known_contracts = self.known_contracts();
        let missing = footprint
            .read_only
            .iter()
            .chain(footprint.read_write.iter())
            .map(|key| {
                let id = key_id(key)?;
                Ok((!self.coverage.contains_key(&id)
                    && !is_local_key(key, &known_contracts, &self.local_code_hashes, false))
                .then_some(id))
            })
            .collect::<Result<Vec<_>, StrictForkError>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(unknown(missing).into());
        }

        // Fee and sequence-number processing, before the operation applies.
        let counts = footprint_counts(footprint, restored);
        let (non_refundable, _) =
            config.resource_fees(resources, counts, transaction_size_bytes, 0);
        let inclusion_charged = i64::from(call.inclusion_fee).min(BASE_FEE);
        let charged = data.resource_fee.saturating_add(inclusion_charged);
        let max_refundable = data.resource_fee.saturating_sub(non_refundable);
        let before = self.ledger_snapshot();
        let before_digest = canonical_ledger_digest(&before)?;
        let mut charged_state = before.clone();
        let source_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: call.source.clone(),
        });
        let charged_account = charge_source(account, charged, &next);
        set_entry(
            &mut charged_state,
            &source_key,
            Some((charged_account.clone(), None)),
        )?;
        let mut state = self.coverage.clone();
        state.insert(
            key_id(&source_key)?,
            LookupState::Present(Rc::new(charged_account), None),
        );

        let mut report = ResourceReport {
            footprint_entries: counts.keys,
            disk_read_entries: counts.disk_reads,
            write_entries: counts.writes,
            transaction_size_bytes,
            fee: ResourceFee {
                non_refundable,
                refundable: 0,
                rent: 0,
            },
            ..ResourceReport::default()
        };
        let mut diagnostics = Vec::new();
        let finish = |snapshot: &LedgerSnapshot,
                      refund: i64,
                      failure: Option<Failure>,
                      events: Vec<DetachedEvent>,
                      diagnostics: Vec<DetachedEvent>,
                      report: ResourceReport|
         -> Result<Executed, TransactionError> {
            let mut after = snapshot.clone();
            refund_source(&mut after, &source_key, refund);
            after.sequence_number = next.sequence_number;
            after.timestamp = next.timestamp;
            let state_changes = state_diff(&before, &after)?;
            let (outcome, result, exceeded, disposition) = match failure {
                None => (
                    InvokeOutcome::Success(ScVal::Void),
                    InvokeHostFunctionResultCode::Success,
                    None,
                    ReceiptDisposition::Committed,
                ),
                Some(failure) => (
                    failure.outcome,
                    failure.result,
                    failure.exceeded,
                    ReceiptDisposition::Rejected,
                ),
            };
            Ok(Executed {
                receipt: TransactionReceipt {
                    call: call.clone(),
                    authorization: transaction.authorization.clone(),
                    transaction_data: data.clone(),
                    outcome,
                    result,
                    exceeded,
                    events,
                    diagnostics,
                    state_changes,
                    before_digest,
                    after_digest: canonical_ledger_digest(&after)?,
                    disposition,
                    ledger_sequence: next.sequence_number,
                    fee_charged: charged.saturating_sub(refund),
                    resources: report,
                },
                after,
            })
        };
        let reject = |failure: Failure, diagnostics: Vec<DetachedEvent>, report: ResourceReport| {
            finish(
                &charged_state,
                max_refundable,
                Some(failure),
                Vec::new(),
                diagnostics,
                report,
            )
        };

        let read = match Self::footprint_read(&state, footprint, restored, &next, &limits)? {
            Ok(read) => read,
            Err(failure) => return reject(failure, Vec::new(), report),
        };
        report.disk_read_bytes = read.disk_read_bytes;
        if read.disk_read_bytes > resources.disk_read_bytes {
            return reject(
                Failure::limit(ResourceExcess {
                    resource: TransactionResource::DiskReadBytes,
                    limit: u64::from(resources.disk_read_bytes),
                    used: u64::from(read.disk_read_bytes),
                }),
                Vec::new(),
                report,
            );
        }

        let budget = config.budget(u64::from(resources.instructions))?;
        let encoded_auth = transaction
            .authorization
            .iter()
            .map(encode)
            .collect::<Result<Vec<_>, _>>()?;
        let result = invoke_host_function(
            &budget,
            true,
            encode(&call.function)?,
            encode(resources)?,
            restored,
            encode(&call.source)?,
            encoded_auth.into_iter(),
            next.clone(),
            read.entries.into_iter(),
            self.prng_seed(call)?.to_vec(),
            &mut diagnostics,
            None,
            module_cache(&next, &read.cached_code),
        );
        let diagnostics = detached_diagnostics(diagnostics);
        report.instructions = budget.get_cpu_insns_consumed().unwrap_or(0);
        report.memory_bytes = budget.get_mem_bytes_consumed().unwrap_or(0);
        let host_failure = |error: &HostError, report: &ResourceReport| {
            let mut failure = Failure::code(InvokeHostFunctionResultCode::Trapped);
            failure.outcome = failure_outcome(error);
            if ScError::try_from(error.error).ok()
                == Some(ScError::Budget(ScErrorCode::ExceededLimit))
            {
                failure.result = InvokeHostFunctionResultCode::ResourceLimitExceeded;
                failure.exceeded = Some(
                    if report.instructions >= u64::from(resources.instructions) {
                        ResourceExcess {
                            resource: TransactionResource::Instructions,
                            limit: u64::from(resources.instructions),
                            used: report.instructions,
                        }
                    } else {
                        ResourceExcess {
                            resource: TransactionResource::MemoryBytes,
                            limit: config.tx_memory_limit(),
                            used: report.memory_bytes,
                        }
                    },
                );
            }
            failure
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                let failure = host_failure(&error, &report);
                return reject(failure, diagnostics, report);
            }
        };
        let return_value = match result.encoded_invoke_result {
            Ok(encoded) => encoded,
            Err(error) => {
                let failure = host_failure(&error, &report);
                return reject(failure, diagnostics, report);
            }
        };

        report.write_bytes =
            match check_writes(&result.ledger_changes, &limits, resources.write_bytes) {
                Ok(written) => written,
                Err(excess) => return reject(Failure::limit(excess), diagnostics, report),
            };
        let events_size = result
            .encoded_contract_events
            .iter()
            .map(Vec::len)
            .sum::<usize>()
            + return_value.len();
        report.contract_events_size_bytes = u32::try_from(events_size).unwrap_or(u32::MAX);
        if let Some(excess) = check_events(events_size as u64, &limits) {
            return reject(Failure::limit(excess), diagnostics, report);
        }
        let rent = config.rent_fee(
            &extract_rent_changes(&result.ledger_changes),
            next.sequence_number,
        );
        let (_, events_fee) = config.resource_fees(
            resources,
            counts,
            transaction_size_bytes,
            report.contract_events_size_bytes,
        );
        let consumed = rent.saturating_add(events_fee);
        if consumed > max_refundable {
            return reject(
                Failure::code(InvokeHostFunctionResultCode::InsufficientRefundableFee),
                diagnostics,
                report,
            );
        }
        report.fee.refundable = consumed;
        report.fee.rent = rent;

        let value = ScVal::from_xdr(return_value, Limits::none())
            .map_err(|_| StrictForkError::HostInspection)?;
        let events = result
            .encoded_contract_events
            .iter()
            .map(|encoded| {
                ContractEvent::from_xdr(encoded, Limits::none())
                    .map(|event| DetachedEvent {
                        event,
                        failed_call: false,
                    })
                    .map_err(|_| StrictForkError::HostInspection)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let applied = apply_ledger_changes(&charged_state, &result.ledger_changes)?;
        let mut executed = finish(
            &applied,
            max_refundable.saturating_sub(consumed),
            None,
            events,
            diagnostics,
            report,
        )?;
        executed.receipt.outcome = InvokeOutcome::Success(value);
        Ok(executed)
    }

    /// Footprint entries from `state` in Host order, or the operation
    /// failure Core reports for an archived entry not marked for restore or
    /// an entry above its size limit.
    fn footprint_read(
        state: &BTreeMap<KeyId, LookupState>,
        footprint: &LedgerFootprint,
        restored: &[u32],
        next: &LedgerInfo,
        limits: &TransactionLimits,
    ) -> Result<Result<FootprintRead, Failure>, TransactionError> {
        let rent_budget = Budget::default();
        let mut read = FootprintRead {
            entries: Vec::new(),
            disk_read_bytes: 0,
            cached_code: Vec::new(),
        };
        let keys = footprint.read_only.iter().map(|key| (key, false)).chain(
            footprint.read_write.iter().enumerate().map(|(index, key)| {
                (
                    key,
                    u32::try_from(index).is_ok_and(|index| restored.contains(&index)),
                )
            }),
        );
        for (key, is_restored) in keys {
            let Some(LookupState::Present(entry, live_until)) = state.get(&key_id(key)?) else {
                read.entries.push((None, None));
                continue;
            };
            let encoded = encode(entry.as_ref())?;
            let size = u32::try_from(encoded.len()).unwrap_or(u32::MAX);
            if !is_soroban_key(key) {
                read.disk_read_bytes = read.disk_read_bytes.saturating_add(size);
                read.entries.push((Some(encoded), None));
                continue;
            }
            let mut live_until_ledger = live_until.ok_or(StrictForkError::HostInspection)?;
            let archived = live_until_ledger < next.sequence_number;
            if archived && !validate::is_persistent_key(key) {
                // An expired temporary entry no longer exists.
                read.entries.push((None, None));
                continue;
            }
            if archived && !is_restored {
                return Ok(Err(Failure::code(
                    InvokeHostFunctionResultCode::EntryArchived,
                )));
            }
            if is_restored && archived {
                // Core restores the entry before the Host runs: it lives for
                // the minimum persistent TTL from the applying ledger.
                read.disk_read_bytes = read.disk_read_bytes.saturating_add(size);
                live_until_ledger = next
                    .min_live_until_ledger_checked(ContractDataDurability::Persistent)
                    .ok_or(TransactionError::InvalidNetworkConfig)?;
            } else if let Some(excess) = check_entry_size(key, encoded.len(), limits) {
                return Ok(Err(Failure::limit(excess)));
            }
            if let LedgerEntryData::ContractCode(code) = &entry.data {
                if !archived {
                    read.cached_code.push(code.code.to_vec());
                }
            }
            read.entries.push((
                Some(encoded),
                Some(TtlLedgerEntryMeta {
                    live_until_ledger,
                    entry_size_for_rent: entry_size_for_rent(&rent_budget, entry, size)
                        .map_err(|_| StrictForkError::HostInspection)?,
                }),
            ));
        }
        Ok(Ok(read))
    }
}

/// Core's fee and sequence-number processing of the source account: the fee
/// is deducted, the sequence number advances, and the account gains the
/// `AccountEntryExtensionV3` that records the ledger and time of that
/// sequence number.
fn charge_source(account: AccountEntry, fee: i64, next: &LedgerInfo) -> LedgerEntry {
    let mut account = account;
    account.balance = account.balance.saturating_sub(fee.min(account.balance));
    account.seq_num = SequenceNumber(account.seq_num.0.saturating_add(1));
    let v3 = AccountEntryExtensionV3 {
        ext: ExtensionPoint::V0,
        seq_ledger: next.sequence_number,
        seq_time: TimePoint(next.timestamp),
    };
    let signers = account.signers.len();
    let mut v1 = match std::mem::replace(&mut account.ext, AccountEntryExt::V0) {
        AccountEntryExt::V0 => AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        },
        AccountEntryExt::V1(v1) => v1,
    };
    let mut v2 = match std::mem::replace(&mut v1.ext, AccountEntryExtensionV1Ext::V0) {
        AccountEntryExtensionV1Ext::V0 => AccountEntryExtensionV2 {
            num_sponsored: 0,
            num_sponsoring: 0,
            signer_sponsoring_i_ds: vec![
                soroban_env_host::xdr::SponsorshipDescriptor(None);
                signers
            ]
            .try_into()
            .unwrap_or_default(),
            ext: AccountEntryExtensionV2Ext::V0,
        },
        AccountEntryExtensionV1Ext::V2(v2) => v2,
    };
    v2.ext = AccountEntryExtensionV2Ext::V3(v3);
    v1.ext = AccountEntryExtensionV1Ext::V2(v2);
    account.ext = AccountEntryExt::V1(v1);
    LedgerEntry {
        last_modified_ledger_seq: next.sequence_number,
        data: LedgerEntryData::Account(account),
        ext: LedgerEntryExt::V0,
    }
}

fn refund_source(snapshot: &mut LedgerSnapshot, source_key: &LedgerKey, refund: i64) {
    if refund <= 0 {
        return;
    }
    let Some((_, (entry, _))) = snapshot
        .ledger_entries
        .iter_mut()
        .find(|(key, _)| key.as_ref() == source_key)
    else {
        // A merged source gets no refund, as in Core.
        return;
    };
    if let LedgerEntryData::Account(account) = &mut entry.data {
        account.balance = account.balance.saturating_add(refund);
    }
}

/// A failed transaction may change only the source account's balance, by
/// the fee charged, and its sequence number, in exactly one more ledger.
fn prove_fee_only_change(
    before: &LedgerSnapshot,
    after: &LedgerSnapshot,
    source: &AccountId,
    receipt: &TransactionReceipt,
) -> Result<(), TransactionError> {
    let source_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: source.clone(),
    });
    let changes = state_diff(before, after)?;
    let [change] = changes.as_slice() else {
        return Err(TransactionError::NotAtomic);
    };
    let accounts = (
        change.before.as_ref().map(|value| &value.entry.data),
        change.after.as_ref().map(|value| &value.entry.data),
    );
    let (Some(LedgerEntryData::Account(old)), Some(LedgerEntryData::Account(new))) = accounts
    else {
        return Err(TransactionError::NotAtomic);
    };
    let fee_only = change.key == source_key
        && new.balance == old.balance.saturating_sub(receipt.fee_charged)
        && new.seq_num.0 == old.seq_num.0.saturating_add(1)
        && after.sequence_number == before.sequence_number.saturating_add(1)
        && receipt.disposition == ReceiptDisposition::Rejected;
    if fee_only {
        Ok(())
    } else {
        Err(TransactionError::NotAtomic)
    }
}

/// The live-until ledger the Host gives a new entry created at `ledger`.
fn new_entry_live_until(ledger: &LedgerInfo, key: &LedgerKey) -> Option<u32> {
    let ttl = match key {
        LedgerKey::ContractCode(_) => ledger.min_persistent_entry_ttl,
        LedgerKey::ContractData(data) => match data.durability {
            ContractDataDurability::Persistent => ledger.min_persistent_entry_ttl,
            ContractDataDurability::Temporary => ledger.min_temp_entry_ttl,
        },
        _ => return None,
    };
    Some(ledger.sequence_number.saturating_add(ttl.saturating_sub(1)))
}

fn set_entry(
    snapshot: &mut LedgerSnapshot,
    key: &LedgerKey,
    value: Option<(LedgerEntry, Option<u32>)>,
) -> Result<(), StrictForkError> {
    let id = key_id(key)?;
    let index = snapshot
        .ledger_entries
        .iter()
        .position(|(candidate, _)| key_id(candidate).is_ok_and(|candidate| candidate == id));
    match (value, index) {
        (Some((entry, live_until)), Some(index)) => {
            snapshot.ledger_entries[index].1 = (Box::new(entry), live_until);
        }
        (Some((entry, live_until)), None) => {
            snapshot
                .ledger_entries
                .push((Box::new(key.clone()), (Box::new(entry), live_until)));
        }
        (None, Some(index)) => {
            snapshot.ledger_entries.remove(index);
        }
        (None, None) => {}
    }
    Ok(())
}

pub(crate) fn contract_address(
    network_id: [u8; 32],
    deployer: &ScAddress,
    salt: [u8; 32],
) -> Result<ScAddress, StrictForkError> {
    let preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
        network_id: Hash(network_id),
        contract_id_preimage: ContractIdPreimage::Address(ContractIdPreimageFromAddress {
            address: deployer.clone(),
            salt: Uint256(salt),
        }),
    });
    Ok(ScAddress::Contract(ContractId(Hash(
        Sha256::digest(encode(&preimage)?).into(),
    ))))
}

fn unknown(keys: Vec<KeyId>) -> StrictForkError {
    StrictForkError::UnknownLedgerKeys {
        count: keys.len(),
        keys,
    }
}

fn encoded_events_size<'a>(
    events: impl Iterator<Item = &'a ContractEvent>,
    return_value: Option<&ScVal>,
) -> Result<u32, StrictForkError> {
    let mut size = 0_usize;
    for event in events {
        size += encode(event)?.len();
    }
    if let Some(value) = return_value {
        size += encode(value)?.len();
    }
    Ok(u32::try_from(size).unwrap_or(u32::MAX))
}

/// Stellar Core keeps deployed WASM parsed outside each transaction's budget.
/// A module that fails to parse is left out, so the Host parses it within the
/// budget and fails naturally.
fn module_cache(ledger: &LedgerInfo, code: &[Vec<u8>]) -> Option<ModuleCache> {
    let compiler = Host::default();
    compiler.budget_cloned().reset_unlimited().ok()?;
    compiler.set_ledger_info(ledger.clone()).ok()?;
    let cache = ModuleCache::new(&compiler).ok()?;
    for wasm in code {
        let _ = cache.parse_and_cache_module_simple(&compiler, ledger.protocol_version, wasm);
    }
    Some(cache)
}

/// Applies Host ledger changes to a copy of `before`.
fn apply_ledger_changes(
    before: &LedgerSnapshot,
    changes: &[LedgerEntryChange],
) -> Result<LedgerSnapshot, StrictForkError> {
    let mut after = before.clone();
    for change in changes {
        let key = LedgerKey::from_xdr(&change.encoded_key, Limits::none())
            .map_err(|_| StrictForkError::HostInspection)?;
        let index = after
            .ledger_entries
            .iter()
            .position(|(candidate, _)| candidate.as_ref() == &key);
        let live_until = change
            .ttl_change
            .as_ref()
            .map(|ttl| ttl.new_live_until_ledger);
        if change.read_only {
            if let (Some(index), Some(live_until)) = (index, live_until) {
                after.ledger_entries[index].1 .1 = Some(live_until);
            }
        } else if let Some(encoded) = &change.encoded_new_value {
            let entry = LedgerEntry::from_xdr(encoded, Limits::none())
                .map_err(|_| StrictForkError::HostInspection)?;
            let value = (Box::new(entry), live_until);
            if let Some(index) = index {
                after.ledger_entries[index].1 = value;
            } else {
                after.ledger_entries.push((Box::new(key), value));
            }
        } else if let Some(index) = index {
            after.ledger_entries.remove(index);
        }
    }
    Ok(after)
}

/// The transaction envelope `call` describes, with zero signature bytes and
/// a zero sequence number; only its size is used.
fn transaction_size(
    call: &TransactionCall,
    authorization: &[SorobanAuthorizationEntry],
    data: &SorobanTransactionData,
) -> Result<u32, TransactionError> {
    let AccountId(PublicKey::PublicKeyTypeEd25519(key)) = &call.source;
    let source_account = match call.muxed_id {
        Some(id) => MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
            id,
            ed25519: key.clone(),
        }),
        None => MuxedAccount::Ed25519(key.clone()),
    };
    let fee = i64::from(call.inclusion_fee).saturating_add(data.resource_fee);
    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: Transaction {
            source_account,
            fee: u32::try_from(fee).unwrap_or(u32::MAX),
            seq_num: SequenceNumber(0),
            cond: call
                .time_bounds
                .clone()
                .map_or(Preconditions::None, Preconditions::Time),
            memo: call.memo.clone(),
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    host_function: call.function.clone(),
                    auth: xdr_vec(authorization.to_vec())?,
                }),
            }]
            .try_into()
            .map_err(|_| StrictForkError::Xdr)?,
            ext: TransactionExt::V1(data.clone()),
        },
        signatures: vec![
            DecoratedSignature {
                hint: SignatureHint([0; 4]),
                signature: Signature(vec![0; 64].try_into().map_err(|_| StrictForkError::Xdr)?),
            };
            usize::from(call.signature_count)
        ]
        .try_into()
        .map_err(|_| StrictForkError::Xdr)?,
    });
    Ok(u32::try_from(encode(&envelope)?.len()).unwrap_or(u32::MAX))
}

fn empty_transaction_data() -> SorobanTransactionData {
    SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        },
        resource_fee: 0,
    }
}

fn credentials_address(credentials: &SorobanCredentials, source: &AccountId) -> ScAddress {
    match credentials {
        SorobanCredentials::SourceAccount => ScAddress::Account(source.clone()),
        SorobanCredentials::Address(credentials) | SorobanCredentials::AddressV2(credentials) => {
            credentials.address.clone()
        }
        SorobanCredentials::AddressWithDelegates(credentials) => {
            credentials.address_credentials.address.clone()
        }
    }
}

fn failure_outcome(error: &HostError) -> InvokeOutcome {
    InvokeOutcome::Failure {
        error: ScError::try_from(error.error).ok(),
        kind: if error.error.is_type(ScErrorType::Contract) {
            InvokeErrorKind::Contract(error.error.get_code())
        } else {
            InvokeErrorKind::Abort
        },
    }
}

fn detached_diagnostics(events: Vec<DiagnosticEvent>) -> Vec<DetachedEvent> {
    events
        .into_iter()
        .filter(|event| event.event.type_ == ContractEventType::Diagnostic)
        .map(|event| DetachedEvent {
            event: event.event,
            failed_call: !event.in_successful_contract_call,
        })
        .collect()
}

/// The Stellar account contract's signature value: a vector of
/// `{public_key: BytesN<32>, signature: BytesN<64>}` maps.
fn account_signature(public_key: [u8; 32], signature: [u8; 64]) -> Result<ScVal, StrictForkError> {
    let symbol = |name: &str| -> Result<ScVal, StrictForkError> {
        Ok(ScVal::Symbol(ScSymbol(
            name.try_into().map_err(|_| StrictForkError::Xdr)?,
        )))
    };
    let bytes = |value: &[u8]| -> Result<ScVal, StrictForkError> {
        Ok(ScVal::Bytes(ScBytes(
            value
                .to_vec()
                .try_into()
                .map_err(|_| StrictForkError::Xdr)?,
        )))
    };
    let map = ScMap(
        vec![
            ScMapEntry {
                key: symbol("public_key")?,
                val: bytes(&public_key)?,
            },
            ScMapEntry {
                key: symbol("signature")?,
                val: bytes(&signature)?,
            },
        ]
        .try_into()
        .map_err(|_| StrictForkError::Xdr)?,
    );
    Ok(ScVal::Vec(Some(ScVec(
        vec![ScVal::Map(Some(map))]
            .try_into()
            .map_err(|_| StrictForkError::Xdr)?,
    ))))
}

/// Flips one bit of the first `Bytes` value inside a signature.
fn tamper_signature(value: &mut ScVal) -> bool {
    match value {
        ScVal::Bytes(bytes) if !bytes.is_empty() => {
            let mut raw = bytes.to_vec();
            raw[0] ^= 1;
            match raw.try_into() {
                Ok(tampered) => {
                    *bytes = ScBytes(tampered);
                    true
                }
                Err(_) => false,
            }
        }
        ScVal::Vec(Some(values)) => {
            let mut items = values.to_vec();
            let tampered = items.iter_mut().any(tamper_signature);
            if tampered {
                if let Ok(updated) = items.try_into() {
                    *values = ScVec(updated);
                }
            }
            tampered
        }
        ScVal::Map(Some(map)) => {
            let mut entries = map.to_vec();
            // Tamper the signature field, never the public key.
            let tampered = entries
                .iter_mut()
                .filter(|entry| {
                    matches!(&entry.key, ScVal::Symbol(name) if name.as_slice() == b"signature")
                })
                .any(|entry| tamper_signature(&mut entry.val));
            if tampered {
                if let Ok(updated) = entries.try_into() {
                    *map = ScMap(updated);
                }
            }
            tampered
        }
        _ => false,
    }
}

fn xdr_vec<T>(values: Vec<T>) -> Result<VecM<T>, StrictForkError> {
    values
        .try_into()
        .map_err(|_| StrictForkError::InvalidInvocationXdr)
}

fn encode<T: WriteXdr>(value: &T) -> Result<Vec<u8>, StrictForkError> {
    value
        .to_xdr(Limits::none())
        .map_err(|_| StrictForkError::Xdr)
}
