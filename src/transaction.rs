//! Transaction-faithful execution on a [`StrictFork`].
//!
//! [`StrictFork::invoke`] runs one contract call in an SDK test environment.
//! The methods in this module instead run a Soroban host function the way
//! Stellar Core applies an `InvokeHostFunctionOp`: through the Host's own
//! end-to-end entry points, under the captured network cost model, with a
//! declared footprint and resources, signed authorization entries, and the
//! network's per-transaction limits.

use std::{collections::BTreeSet, fmt, rc::Rc};

use ed25519_dalek::{Signer as _, SigningKey};
use sha2::{Digest, Sha256};
use soroban_env_host::{
    budget::Budget,
    e2e_invoke::{
        entry_size_for_rent, extract_rent_changes, invoke_host_function,
        invoke_host_function_in_recording_mode, LedgerEntryChange, RecordingInvocationAuthMode,
        TtlLedgerEntryMeta,
    },
    fees::{
        compute_rent_fee, compute_rent_write_fee_per_1kb, compute_transaction_resource_fee,
        FeeConfiguration, LedgerEntryRentChange, RentFeeConfiguration, RentWriteFeeConfiguration,
        TransactionResources,
    },
    xdr::{
        AccountEntry, AccountEntryExt, AccountId, ConfigSettingContractBandwidthV0,
        ConfigSettingContractComputeV0, ConfigSettingContractEventsV0,
        ConfigSettingContractHistoricalDataV0, ConfigSettingContractLedgerCostExtV0,
        ConfigSettingContractLedgerCostV0, ConfigSettingEntry, ConfigSettingId, ContractCostParams,
        ContractDataDurability, ContractDataEntry, ContractEvent, ContractEventType,
        ContractExecutable, ContractId, ContractIdPreimage, ContractIdPreimageFromAddress,
        CreateContractArgs, CreateContractArgsV2, DecoratedSignature, DiagnosticEvent,
        ExtensionPoint, Hash, HashIdPreimage, HashIdPreimageContractId,
        HashIdPreimageSorobanAuthorizationWithAddress, HostFunction, InvokeContractArgs,
        InvokeHostFunctionOp, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerFootprint,
        LedgerKey, LedgerKeyAccount, LedgerKeyConfigSetting, LedgerKeyContractData, Limits, Memo,
        MuxedAccount, Operation, OperationBody, Preconditions, PublicKey, ReadXdr, ScAddress,
        ScBytes, ScError, ScErrorType, ScMap, ScMapEntry, ScSymbol, ScVal, ScVec, SequenceNumber,
        Signature, SignatureHint, SorobanAddressCredentials, SorobanAuthorizationEntry,
        SorobanCredentials, SorobanResources, SorobanResourcesExtV0, SorobanTransactionData,
        SorobanTransactionDataExt, StateArchivalSettings, String32, Thresholds, Transaction,
        TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, WriteXdr,
    },
    Host, HostError, LedgerInfo, ModuleCache,
};
use soroban_ledger_snapshot::LedgerSnapshot;
use thiserror::Error;

use crate::{
    canonical_ledger_digest,
    capture::LookupState,
    strict::{is_local_key, key_id, state_diff, strict_env, update_coverage, StrictSource},
    AuthorizationTree, DetachedEvent, InvokeErrorKind, InvokeOutcome, InvokeRequest, LedgerValue,
    Receipt, ReceiptDisposition, StateChange, StrictFork, StrictForkError,
};

const PRNG_SEED_DOMAIN: &[u8] = b"KANATOKO\0TRANSACTION-PRNG\0V1\0";
/// Recording mode can measure slightly fewer instructions than the enforcing
/// run; prepared transactions declare this much more, as RPC simulation does.
const INSTRUCTION_MARGIN_PERCENT: u64 = 4;
const INSTRUCTION_MARGIN_MINIMUM: u64 = 50_000;

/// An Ed25519 key that signs Soroban authorization entries and names a
/// transaction source account.
///
/// `Debug` output shows only the public key.
#[derive(Clone)]
pub struct Ed25519Signer {
    key: SigningKey,
}

impl Ed25519Signer {
    /// Creates a signer from a 32-byte Ed25519 secret seed.
    ///
    /// Use fixed test seeds only. A key that controls a real account must
    /// never be placed in a test.
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
        }
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

/// Soroban network configuration read from captured `ConfigSetting` entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkConfig {
    contract_max_size_bytes: u32,
    compute: ConfigSettingContractComputeV0,
    ledger_cost: ConfigSettingContractLedgerCostV0,
    ledger_cost_ext: ConfigSettingContractLedgerCostExtV0,
    historical: ConfigSettingContractHistoricalDataV0,
    events: ConfigSettingContractEventsV0,
    bandwidth: ConfigSettingContractBandwidthV0,
    cpu_cost_params: ContractCostParams,
    memory_cost_params: ContractCostParams,
    data_key_size_bytes: u32,
    data_entry_size_bytes: u32,
    state_archival: StateArchivalSettings,
    live_state_size_window: Vec<u64>,
}

impl NetworkConfig {
    /// `ConfigSetting` entries a fixture must contain for transaction-faithful
    /// execution. [`crate::CaptureBuilder::capture_network_config`] and
    /// [`crate::AutoRunner::capture_network_config`] capture exactly these.
    pub const REQUIRED_SETTINGS: [ConfigSettingId; 13] = [
        ConfigSettingId::ContractMaxSizeBytes,
        ConfigSettingId::ContractComputeV0,
        ConfigSettingId::ContractLedgerCostV0,
        ConfigSettingId::ContractHistoricalDataV0,
        ConfigSettingId::ContractEventsV0,
        ConfigSettingId::ContractBandwidthV0,
        ConfigSettingId::ContractCostParamsCpuInstructions,
        ConfigSettingId::ContractCostParamsMemoryBytes,
        ConfigSettingId::ContractDataKeySizeBytes,
        ConfigSettingId::ContractDataEntrySizeBytes,
        ConfigSettingId::StateArchival,
        ConfigSettingId::LiveSorobanStateSizeWindow,
        ConfigSettingId::ContractLedgerCostExtV0,
    ];

    fn from_lookup(
        mut lookup: impl FnMut(&LedgerKey) -> Option<Rc<LedgerEntry>>,
    ) -> Result<Self, TransactionError> {
        let mut settings = Vec::new();
        let mut missing = Vec::new();
        for id in Self::REQUIRED_SETTINGS {
            let entry = lookup(&config_setting_key(id));
            match entry.as_deref().map(|entry| &entry.data) {
                Some(LedgerEntryData::ConfigSetting(setting)) if setting.discriminant() == id => {
                    settings.push(setting.clone());
                }
                _ => missing.push(id),
            }
        }
        if !missing.is_empty() {
            return Err(TransactionError::MissingNetworkConfig { missing });
        }
        let mut config = Self {
            contract_max_size_bytes: 0,
            compute: ConfigSettingContractComputeV0 {
                ledger_max_instructions: 0,
                tx_max_instructions: 0,
                fee_rate_per_instructions_increment: 0,
                tx_memory_limit: 0,
            },
            ledger_cost: zero_ledger_cost(),
            ledger_cost_ext: ConfigSettingContractLedgerCostExtV0 {
                tx_max_footprint_entries: 0,
                fee_write1_kb: 0,
            },
            historical: ConfigSettingContractHistoricalDataV0 {
                fee_historical1_kb: 0,
            },
            events: ConfigSettingContractEventsV0 {
                tx_max_contract_events_size_bytes: 0,
                fee_contract_events1_kb: 0,
            },
            bandwidth: ConfigSettingContractBandwidthV0 {
                ledger_max_txs_size_bytes: 0,
                tx_max_size_bytes: 0,
                fee_tx_size1_kb: 0,
            },
            cpu_cost_params: ContractCostParams::default(),
            memory_cost_params: ContractCostParams::default(),
            data_key_size_bytes: 0,
            data_entry_size_bytes: 0,
            state_archival: StateArchivalSettings {
                max_entry_ttl: 0,
                min_temporary_ttl: 0,
                min_persistent_ttl: 0,
                persistent_rent_rate_denominator: 0,
                temp_rent_rate_denominator: 0,
                max_entries_to_archive: 0,
                live_soroban_state_size_window_sample_size: 0,
                live_soroban_state_size_window_sample_period: 0,
                eviction_scan_size: 0,
                starting_eviction_scan_level: 0,
            },
            live_state_size_window: Vec::new(),
        };
        for setting in settings {
            match setting {
                ConfigSettingEntry::ContractMaxSizeBytes(value) => {
                    config.contract_max_size_bytes = value;
                }
                ConfigSettingEntry::ContractComputeV0(value) => config.compute = value,
                ConfigSettingEntry::ContractLedgerCostV0(value) => config.ledger_cost = value,
                ConfigSettingEntry::ContractLedgerCostExtV0(value) => {
                    config.ledger_cost_ext = value;
                }
                ConfigSettingEntry::ContractHistoricalDataV0(value) => config.historical = value,
                ConfigSettingEntry::ContractEventsV0(value) => config.events = value,
                ConfigSettingEntry::ContractBandwidthV0(value) => config.bandwidth = value,
                ConfigSettingEntry::ContractCostParamsCpuInstructions(value) => {
                    config.cpu_cost_params = value;
                }
                ConfigSettingEntry::ContractCostParamsMemoryBytes(value) => {
                    config.memory_cost_params = value;
                }
                ConfigSettingEntry::ContractDataKeySizeBytes(value) => {
                    config.data_key_size_bytes = value;
                }
                ConfigSettingEntry::ContractDataEntrySizeBytes(value) => {
                    config.data_entry_size_bytes = value;
                }
                ConfigSettingEntry::StateArchival(value) => config.state_archival = value,
                ConfigSettingEntry::LiveSorobanStateSizeWindow(value) => {
                    config.live_state_size_window = value.to_vec();
                }
                _ => {}
            }
        }
        Ok(config)
    }

    /// The network's per-transaction Soroban limits.
    #[must_use]
    pub fn limits(&self) -> TransactionLimits {
        TransactionLimits {
            instructions: u64::try_from(self.compute.tx_max_instructions).unwrap_or(0),
            memory_bytes: u64::from(self.compute.tx_memory_limit),
            footprint_entries: self.ledger_cost_ext.tx_max_footprint_entries,
            disk_read_entries: self.ledger_cost.tx_max_disk_read_entries,
            disk_read_bytes: self.ledger_cost.tx_max_disk_read_bytes,
            write_entries: self.ledger_cost.tx_max_write_ledger_entries,
            write_bytes: self.ledger_cost.tx_max_write_bytes,
            contract_events_size_bytes: self.events.tx_max_contract_events_size_bytes,
            transaction_size_bytes: self.bandwidth.tx_max_size_bytes,
            ledger_key_size_bytes: self.data_key_size_bytes,
            ledger_entry_size_bytes: self.data_entry_size_bytes,
            contract_code_size_bytes: self.contract_max_size_bytes,
        }
    }

    /// Average live Soroban state size over the captured sample window, which
    /// sets the rent write fee.
    #[must_use]
    pub fn average_soroban_state_size_bytes(&self) -> u64 {
        let samples = self.live_state_size_window.len() as u64;
        if samples == 0 {
            return 0;
        }
        let total: u128 = self
            .live_state_size_window
            .iter()
            .map(|sample| u128::from(*sample))
            .sum();
        u64::try_from(total / u128::from(samples)).unwrap_or(u64::MAX)
    }

    fn budget(&self, instructions: u64) -> Result<Budget, TransactionError> {
        Budget::try_from_configs(
            instructions,
            u64::from(self.compute.tx_memory_limit),
            self.cpu_cost_params.clone(),
            self.memory_cost_params.clone(),
        )
        .map_err(|_| TransactionError::InvalidNetworkConfig)
    }

    fn fee_configuration(&self) -> FeeConfiguration {
        FeeConfiguration {
            fee_per_instruction_increment: self.compute.fee_rate_per_instructions_increment,
            fee_per_disk_read_entry: self.ledger_cost.fee_disk_read_ledger_entry,
            fee_per_write_entry: self.ledger_cost.fee_write_ledger_entry,
            fee_per_disk_read_1kb: self.ledger_cost.fee_disk_read1_kb,
            fee_per_write_1kb: self.ledger_cost_ext.fee_write1_kb,
            fee_per_historical_1kb: self.historical.fee_historical1_kb,
            fee_per_contract_event_1kb: self.events.fee_contract_events1_kb,
            fee_per_transaction_size_1kb: self.bandwidth.fee_tx_size1_kb,
        }
    }

    fn rent_fee_configuration(&self) -> RentFeeConfiguration {
        let rent_write = compute_rent_write_fee_per_1kb(
            i64::try_from(self.average_soroban_state_size_bytes()).unwrap_or(i64::MAX),
            &RentWriteFeeConfiguration {
                state_target_size_bytes: self.ledger_cost.soroban_state_target_size_bytes,
                rent_fee_1kb_state_size_low: self.ledger_cost.rent_fee1_kb_soroban_state_size_low,
                rent_fee_1kb_state_size_high: self.ledger_cost.rent_fee1_kb_soroban_state_size_high,
                state_size_rent_fee_growth_factor: self
                    .ledger_cost
                    .soroban_state_rent_fee_growth_factor,
            },
        );
        RentFeeConfiguration {
            fee_per_write_1kb: self.ledger_cost_ext.fee_write1_kb,
            fee_per_rent_1kb: rent_write,
            fee_per_write_entry: self.ledger_cost.fee_write_ledger_entry,
            persistent_rent_rate_denominator: self.state_archival.persistent_rent_rate_denominator,
            temporary_rent_rate_denominator: self.state_archival.temp_rent_rate_denominator,
        }
    }

    /// Non-refundable fee of the declared resources, plus the refundable fee
    /// of the measured events and rent.
    fn resource_fee(
        &self,
        declared: &SorobanResources,
        counts: FootprintCounts,
        transaction_size_bytes: u32,
        contract_events_size_bytes: u32,
        rent_changes: &[LedgerEntryRentChange],
        ledger_sequence: u32,
    ) -> ResourceFee {
        let (non_refundable, events) = compute_transaction_resource_fee(
            &TransactionResources {
                instructions: declared.instructions,
                disk_read_entries: counts.disk_reads,
                write_entries: counts.writes,
                disk_read_bytes: declared.disk_read_bytes,
                write_bytes: declared.write_bytes,
                contract_events_size_bytes,
                transaction_size_bytes,
            },
            &self.fee_configuration(),
        );
        let rent = compute_rent_fee(
            rent_changes,
            &self.rent_fee_configuration(),
            ledger_sequence,
        );
        ResourceFee {
            non_refundable,
            refundable: events.saturating_add(rent),
            rent,
        }
    }
}

fn zero_ledger_cost() -> ConfigSettingContractLedgerCostV0 {
    ConfigSettingContractLedgerCostV0 {
        ledger_max_disk_read_entries: 0,
        ledger_max_disk_read_bytes: 0,
        ledger_max_write_ledger_entries: 0,
        ledger_max_write_bytes: 0,
        tx_max_disk_read_entries: 0,
        tx_max_disk_read_bytes: 0,
        tx_max_write_ledger_entries: 0,
        tx_max_write_bytes: 0,
        fee_disk_read_ledger_entry: 0,
        fee_write_ledger_entry: 0,
        fee_disk_read1_kb: 0,
        soroban_state_target_size_bytes: 0,
        rent_fee1_kb_soroban_state_size_low: 0,
        rent_fee1_kb_soroban_state_size_high: 0,
        soroban_state_rent_fee_growth_factor: 0,
    }
}

fn config_setting_key(id: ConfigSettingId) -> LedgerKey {
    LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: id,
    })
}

/// Ledger keys of [`NetworkConfig::REQUIRED_SETTINGS`].
pub(crate) fn network_config_keys() -> Vec<LedgerKey> {
    NetworkConfig::REQUIRED_SETTINGS
        .into_iter()
        .map(config_setting_key)
        .collect()
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
    /// Encoded `ContractData` and `ContractCode` footprint keys.
    pub ledger_key_size_bytes: u32,
    /// Encoded written `ContractData` entries.
    pub ledger_entry_size_bytes: u32,
    /// WASM size of a written `ContractCode` entry.
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

/// Resource fee in stroops, computed with the Host's fee functions and the
/// captured fee rates.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ResourceFee {
    /// Charged on the declared instructions, ledger entries, read/write
    /// bytes, and transaction size.
    pub non_refundable: i64,
    /// Measured contract events and rent; the unused part of the declared
    /// refundable fee is refunded on the network.
    pub refundable: i64,
    /// Rent part of [`Self::refundable`].
    pub rent: i64,
}

impl ResourceFee {
    /// Minimum `resource_fee` a transaction must declare.
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
    /// Encoded transaction envelope with one source-account signature.
    pub transaction_size_bytes: u32,
    /// Fees of these resources under the captured rates.
    pub fee: ResourceFee,
}

/// A Soroban host function as one `InvokeHostFunctionOp` of a transaction
/// whose source account is controlled by a known key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionCall {
    source: AccountId,
    function: HostFunction,
}

impl TransactionCall {
    /// A call of `function` from the account of `source`.
    ///
    /// The source is a signer rather than a bare account so that
    /// source-account authorization is always backed by a key the test
    /// holds; Kanatoko never authorizes as an account without its key.
    #[must_use]
    pub fn new(source: &Ed25519Signer, function: HostFunction) -> Self {
        Self {
            source: source.account_id(),
            function,
        }
    }

    /// `HostFunction::InvokeContract` for a detached contract call.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::InvalidInvocationXdr`] when the arguments
    /// exceed the XDR vector bound.
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
    /// The `ContractCode` entry of these exact bytes need not be captured:
    /// its content is determined by the WASM, so an uncaptured entry is
    /// treated as absent. When the network already holds the same code, a
    /// network run differs only in rent and TTL.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::InvalidInvocationXdr`] when `wasm` exceeds
    /// the XDR byte bound.
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
    /// [`StrictFork::contract_address`] derives the resulting address. For a
    /// local deployer account, that address cannot exist on the network, so
    /// its ledger keys need not be captured; for any other deployer the
    /// fixture must hold the address's contract instance key.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::InvalidInvocationXdr`] when the arguments
    /// exceed the XDR vector bound.
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

    /// Transaction source account.
    #[must_use]
    pub const fn source(&self) -> &AccountId {
        &self.source
    }

    #[must_use]
    pub const fn function(&self) -> &HostFunction {
        &self.function
    }
}

/// Detached evidence of one simulated, prepared, or applied transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TransactionReceipt {
    pub call: TransactionCall,
    /// Authorization entries the Host recorded (simulation) or was given.
    pub authorization: Vec<SorobanAuthorizationEntry>,
    /// Declared footprint, resources, and resource fee. For a simulation,
    /// the recorded values.
    pub transaction_data: SorobanTransactionData,
    pub outcome: InvokeOutcome,
    pub events: Vec<DetachedEvent>,
    pub diagnostics: Vec<DetachedEvent>,
    pub state_changes: Vec<StateChange>,
    pub before_digest: [u8; 32],
    /// Equals [`Self::before_digest`] for a rejected transaction.
    pub after_digest: [u8; 32],
    pub disposition: ReceiptDisposition,
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
    /// resources, and resource fee.
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

/// How a transaction expected to fail failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum FailedApply {
    /// The Host executed it and the invocation failed.
    Rejected(Box<TransactionReceipt>),
    /// Kanatoko refused it before or after execution, for example for an
    /// exceeded limit or an uncaptured footprint key.
    Refused(Box<TransactionError>),
}

/// Failures of transaction-faithful execution. Every failed method leaves the
/// fork unchanged.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransactionError {
    #[error(transparent)]
    Strict(#[from] StrictForkError),
    #[error(
        "the fixture lacks network ConfigSetting entries {missing:?}; capture it with \
         capture_network_config()"
    )]
    MissingNetworkConfig { missing: Vec<ConfigSettingId> },
    #[error("the captured network configuration is not usable by this Host")]
    InvalidNetworkConfig,
    #[error("{resource:?} of {used} exceeds the network per-transaction limit {limit}")]
    NetworkLimitExceeded {
        resource: TransactionResource,
        limit: u64,
        used: u64,
    },
    #[error("{resource:?} of {used} exceeds the declared {declared}")]
    DeclaredResourceExceeded {
        resource: TransactionResource,
        declared: u64,
        used: u64,
    },
    #[error("declared resource fee {declared} is below the required {required} stroops")]
    InsufficientResourceFee { declared: i64, required: i64 },
    #[error("restored-entry indices do not match the read-write footprint")]
    InvalidTransactionData,
    #[error("the transaction source account does not exist on the fork")]
    SourceAccountMissing,
    #[error("the source key does not meet its account's medium threshold")]
    SourceAccountNotAuthorized,
    #[error("no signer was supplied for an authorization address")]
    MissingSigner { address: ScAddress },
    #[error("authorization nonce overflowed")]
    NonceOverflow,
    #[error("the transaction simulation failed")]
    SimulationFailed(Box<TransactionReceipt>),
    #[error("the transaction expected to fail succeeded and was not committed")]
    UnexpectedSuccess(Box<TransactionReceipt>),
    #[error("a failed transaction changed the fork's ledger digest")]
    NotAtomic,
    #[error("the local account already exists")]
    LocalAccountExists,
    #[error("a new local account needs at least {minimum} stroops")]
    LocalAccountBalanceTooLow { minimum: i64 },
}

impl From<crate::FixtureError> for TransactionError {
    fn from(error: crate::FixtureError) -> Self {
        Self::Strict(StrictForkError::Fixture(error))
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FootprintCounts {
    keys: u32,
    disk_reads: u32,
    writes: u32,
}

/// Result of one execution before the fork decides whether to commit it.
struct Executed {
    receipt: TransactionReceipt,
    after: Option<LedgerSnapshot>,
}

impl StrictFork {
    /// Network ID of the captured network.
    #[must_use]
    pub fn network_id(&self) -> [u8; 32] {
        self.ledger_info().network_id
    }

    /// Current ledger sequence of the fork.
    #[must_use]
    pub fn ledger_sequence(&self) -> u32 {
        self.ledger_info().sequence_number
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
            None => Err(StrictForkError::UnknownLedgerKeys {
                count: 1,
                keys: vec![id],
            }),
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
        let preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
            network_id: Hash(self.network_id()),
            contract_id_preimage: ContractIdPreimage::Address(ContractIdPreimageFromAddress {
                address: deployer.clone(),
                salt: Uint256(salt),
            }),
        });
        Ok(ScAddress::Contract(ContractId(Hash(
            Sha256::digest(encode(&preimage)?).into(),
        ))))
    }

    /// Explicitly injects a new local `AccountEntry` with `balance` stroops,
    /// master weight 1, and zero thresholds.
    ///
    /// This is local ledger injection for test identities, not a Stellar
    /// payment or `CreateAccount` operation. The account's trustlines and
    /// authorization nonces become local knowledge, so later calls can create
    /// them without capture.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::LocalAccountExists`] when the account is
    /// present in the fork and
    /// [`TransactionError::LocalAccountBalanceTooLow`] below two base
    /// reserves.
    pub fn create_local_account(
        &mut self,
        account: &AccountId,
        balance: i64,
    ) -> Result<StateChange, TransactionError> {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account.clone(),
        });
        if matches!(
            self.coverage.get(&key_id(&key)?),
            Some(LookupState::Present(_, _))
        ) {
            return Err(TransactionError::LocalAccountExists);
        }
        let ledger = self.ledger_info();
        let minimum = i64::from(ledger.base_reserve).saturating_mul(2);
        if balance < minimum {
            return Err(TransactionError::LocalAccountBalanceTooLow { minimum });
        }
        let entry = LedgerEntry {
            last_modified_ledger_seq: ledger.sequence_number,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account.clone(),
                balance,
                seq_num: SequenceNumber(i64::from(ledger.sequence_number) << 32),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Vec::new().try_into().unwrap_or_default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        let address = ScAddress::Account(account.clone());
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

    /// Signs `tree` for its address with `signer`.
    ///
    /// The entry uses `SorobanCredentials::AddressV2`, whose payload is the
    /// SHA-256 of `HashIdPreimage::SorobanAuthorizationWithAddress` over this
    /// fork's network ID, and the Stellar account contract's signature shape:
    /// a vector holding one `{public_key, signature}` map. The Host verifies
    /// that `signer` is a signer of the tree's account with enough weight,
    /// that `nonce` is unused, and that `signature_expiration_ledger` has not
    /// passed.
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
        let preimage = HashIdPreimage::SorobanAuthorizationWithAddress(
            HashIdPreimageSorobanAuthorizationWithAddress {
                network_id: Hash(self.network_id()),
                nonce,
                signature_expiration_ledger,
                address: tree.address.clone(),
                invocation: tree.invocation.clone(),
            },
        );
        let payload = Sha256::digest(encode(&preimage)?);
        Ok(SorobanAuthorizationEntry {
            credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
                address: tree.address.clone(),
                nonce,
                signature_expiration_ledger,
                signature: account_signature(signer.public_key(), signer.sign(&payload))?,
            }),
            root_invocation: tree.invocation.clone(),
        })
    }

    /// Authorizes every tree of `simulation`: source-account credentials for
    /// the transaction source and an entry signed by the matching signer for
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
            entries.push(self.sign_authorization(
                tree,
                signer,
                nonce,
                signature_expiration_ledger,
            )?);
        }
        Ok(entries)
    }

    /// Simulates `call` in recording mode, as RPC `simulateTransaction` does,
    /// and returns the authorization trees to sign. The fork never changes.
    ///
    /// Authorization is recorded and mock-satisfied; the receipt's state
    /// changes are what the call would write with that authorization.
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
        let config = self.network_config()?;
        let receipt = self.record(&config, call, None)?;
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

    /// Simulates `call` with the supplied, enforced `authorization` and
    /// returns it with the recorded footprint, declared resources, and the
    /// minimum resource fee. The fork never changes.
    ///
    /// Declared instructions add a margin of 4% (at least 50,000), capped at
    /// the network limit, because recording can measure slightly fewer
    /// instructions than enforcing execution. Read and write bytes are
    /// declared exactly as recorded.
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
        let config = self.network_config()?;
        let receipt = self.record(&config, call, Some(authorization))?;
        if !receipt.outcome.is_success() {
            return Err(TransactionError::SimulationFailed(Box::new(receipt)));
        }
        let limits = config.limits();
        let mut transaction_data = receipt.transaction_data.clone();
        let measured = u64::from(transaction_data.resources.instructions);
        let margin = (measured * INSTRUCTION_MARGIN_PERCENT / 100).max(INSTRUCTION_MARGIN_MINIMUM);
        transaction_data.resources.instructions =
            u32::try_from(measured.saturating_add(margin).min(limits.instructions))
                .unwrap_or(u32::MAX);
        let counts = footprint_counts(
            &transaction_data.resources.footprint,
            restored_indices(&transaction_data),
        );
        let fee_for = |data: &SorobanTransactionData| -> Result<i64, TransactionError> {
            let size = transaction_size(call, &receipt.authorization, data)?;
            Ok(config
                .resource_fee(
                    &data.resources,
                    counts,
                    size,
                    receipt.resources.contract_events_size_bytes,
                    &[],
                    self.ledger_sequence(),
                )
                .non_refundable)
        };
        // Rent is refundable and was measured by the recording run.
        let refundable = receipt.resources.fee.refundable;
        transaction_data.resource_fee = fee_for(&transaction_data)?.saturating_add(refundable);
        Ok(PreparedTransaction {
            call: call.clone(),
            authorization: receipt.authorization,
            transaction_data,
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

    /// Applies `transaction` as Stellar Core applies one
    /// `InvokeHostFunctionOp`, and commits it only on success.
    ///
    /// Before execution, Kanatoko checks that the source account exists and
    /// its master key meets the medium threshold, that every declared
    /// resource, footprint count, footprint key size, and the transaction
    /// size are within the network's per-transaction limits, that the
    /// declared resource fee covers the non-refundable fee, and that every
    /// footprint key is captured or local. The Host then executes with the
    /// enforcing footprint, the declared instructions and the network memory
    /// limit as its budget, the supplied authorization entries, and a module
    /// cache of the footprint's WASM, as Core caches deployed code. A ledger
    /// key outside the footprint fails the invocation, as on the network.
    /// After execution, the measured write bytes must fit the declaration,
    /// events and the return value the network limit, written entries the
    /// entry size limits, and the refundable fee the declared resource fee.
    ///
    /// Fees and sequence numbers are reported, never charged or consumed.
    /// Envelope signatures are not verified: the source is known to be
    /// controlled by a test key from [`TransactionCall::new`].
    ///
    /// A failed invocation returns a [`ReceiptDisposition::Rejected`] receipt
    /// and leaves the fork unchanged.
    ///
    /// # Errors
    ///
    /// Returns a [`TransactionError`] for any check above, missing network
    /// configuration, or an uncaptured footprint key. The fork is unchanged.
    pub fn apply_transaction(
        &mut self,
        transaction: &PreparedTransaction,
    ) -> Result<TransactionReceipt, TransactionError> {
        let executed = self.execute(transaction)?;
        let mut receipt = executed.receipt;
        if let Some(after) = executed.after {
            self.local_code_hashes = self.local_knowledge(&transaction.call)?.1;
            self.commit(&after, &receipt.state_changes)?;
            receipt.disposition = ReceiptDisposition::Committed;
        }
        Ok(receipt)
    }

    /// Applies a `transaction` that must fail and proves that the failure
    /// left the fork's ledger digest unchanged.
    ///
    /// Every [`TransactionError`] of [`Self::apply_transaction`] counts as a
    /// refusal, including missing network configuration, so assert the
    /// expected failure.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::UnexpectedSuccess`] if the transaction
    /// succeeds; it is not committed. Returns [`TransactionError::NotAtomic`]
    /// if the ledger digest changed.
    pub fn expect_failed_apply(
        &mut self,
        transaction: &PreparedTransaction,
    ) -> Result<FailedApply, TransactionError> {
        let before = self.ledger_digest()?;
        let failure = match self.execute(transaction) {
            Err(error) => FailedApply::Refused(Box::new(error)),
            Ok(executed) if executed.after.is_some() => {
                return Err(TransactionError::UnexpectedSuccess(Box::new(
                    executed.receipt,
                )));
            }
            Ok(executed) => {
                if executed.receipt.after_digest != executed.receipt.before_digest {
                    return Err(TransactionError::NotAtomic);
                }
                FailedApply::Rejected(Box::new(executed.receipt))
            }
        };
        if self.ledger_digest()? != before {
            return Err(TransactionError::NotAtomic);
        }
        Ok(failure)
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
        let id = key_id(&key)?;
        let index = after
            .ledger_entries
            .iter()
            .position(|(candidate, _)| key_id(candidate).is_ok_and(|candidate| candidate == id));
        match (value, index) {
            (Some((entry, live_until)), Some(index)) => {
                after.ledger_entries[index].1 = (Box::new(entry), live_until);
            }
            (Some((entry, live_until)), None) => {
                after
                    .ledger_entries
                    .push((Box::new(key.clone()), (Box::new(entry), live_until)));
            }
            (None, Some(index)) => {
                after.ledger_entries.remove(index);
            }
            (None, None) => {}
        }
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
        // A contract created by a transaction has no other ledger data, so all
        // of its keys are local knowledge from now on.
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

    fn source(
        &self,
        call: &TransactionCall,
        allow_mock_nonces: bool,
    ) -> Result<Rc<StrictSource>, TransactionError> {
        let (local_contracts, local_code_hashes) = self.local_knowledge(call)?;
        Ok(Rc::new(StrictSource::new(
            &self.coverage,
            &local_contracts,
            &local_code_hashes,
            allow_mock_nonces,
        )))
    }

    /// Local addresses and code hashes, plus what `call` itself determines:
    /// the `ContractCode` entry of uploaded WASM is fully determined by its
    /// bytes, and a contract that a local account deploys cannot exist on
    /// the network, so neither needs to be captured.
    fn local_knowledge(
        &self,
        call: &TransactionCall,
    ) -> Result<(BTreeSet<ScAddress>, BTreeSet<[u8; 32]>), TransactionError> {
        let mut contracts = self.local_contracts.clone();
        let mut code_hashes = self.local_code_hashes.clone();
        match &call.function {
            HostFunction::UploadContractWasm(wasm) => {
                code_hashes.insert(Sha256::digest(wasm.as_slice()).into());
            }
            HostFunction::CreateContract(CreateContractArgs {
                contract_id_preimage:
                    ContractIdPreimage::Address(ContractIdPreimageFromAddress { address, salt }),
                ..
            })
            | HostFunction::CreateContractV2(CreateContractArgsV2 {
                contract_id_preimage:
                    ContractIdPreimage::Address(ContractIdPreimageFromAddress { address, salt }),
                ..
            }) if matches!(address, ScAddress::Account(_)) && contracts.contains(address) => {
                contracts.insert(self.contract_address(address, salt.0)?);
            }
            _ => {}
        }
        Ok((contracts, code_hashes))
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

    /// Recording-mode run: `authorization` `None` records authorization,
    /// otherwise the entries are enforced.
    #[allow(clippy::too_many_lines)]
    fn record(
        &self,
        config: &NetworkConfig,
        call: &TransactionCall,
        authorization: Option<Vec<SorobanAuthorizationEntry>>,
    ) -> Result<TransactionReceipt, TransactionError> {
        let before = self.ledger_snapshot();
        let before_digest = canonical_ledger_digest(&before)?;
        let ledger = self.ledger_info();
        let budget = config.budget(config.limits().instructions)?;
        let source = self.source(call, authorization.is_none())?;
        let auth_mode = match authorization {
            None => RecordingInvocationAuthMode::recording(true, true),
            Some(entries) => RecordingInvocationAuthMode::Enforcing(entries),
        };
        let mut diagnostics = Vec::new();
        let recorded = invoke_host_function_in_recording_mode(
            &budget,
            true,
            &call.function,
            &call.source,
            auth_mode,
            ledger.clone(),
            source.clone(),
            self.prng_seed(call)?,
            &mut diagnostics,
        );
        let unknown = source.unknown.borrow().clone();
        if !unknown.is_empty() {
            return Err(StrictForkError::UnknownLedgerKeys {
                count: unknown.len(),
                keys: unknown.into_iter().collect(),
            }
            .into());
        }
        let diagnostics = detached_diagnostics(diagnostics);
        let recorded = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                return Ok(TransactionReceipt {
                    call: call.clone(),
                    authorization: Vec::new(),
                    transaction_data: empty_transaction_data(),
                    outcome: failure_outcome(&error),
                    events: Vec::new(),
                    diagnostics,
                    state_changes: Vec::new(),
                    before_digest,
                    after_digest: before_digest,
                    disposition: ReceiptDisposition::Previewed,
                    resources: ResourceReport::default(),
                })
            }
        };
        let transaction_data = SorobanTransactionData {
            ext: if recorded.restored_rw_entry_indices.is_empty() {
                SorobanTransactionDataExt::V0
            } else {
                SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                    archived_soroban_entries: recorded
                        .restored_rw_entry_indices
                        .clone()
                        .try_into()
                        .map_err(|_| TransactionError::InvalidTransactionData)?,
                })
            },
            resources: recorded.resources.clone(),
            resource_fee: 0,
        };
        let counts = footprint_counts(
            &recorded.resources.footprint,
            &recorded.restored_rw_entry_indices,
        );
        let transaction_size_bytes = transaction_size(call, &recorded.auth, &transaction_data)?;
        let fee = config.resource_fee(
            &recorded.resources,
            counts,
            transaction_size_bytes,
            recorded.contract_events_and_return_value_size,
            &extract_rent_changes(&recorded.ledger_changes),
            ledger.sequence_number,
        );
        let resources = ResourceReport {
            instructions: budget.get_cpu_insns_consumed().unwrap_or(0),
            memory_bytes: budget.get_mem_bytes_consumed().unwrap_or(0),
            footprint_entries: counts.keys,
            disk_read_entries: counts.disk_reads,
            disk_read_bytes: recorded.resources.disk_read_bytes,
            write_entries: counts.writes,
            write_bytes: recorded.resources.write_bytes,
            contract_events_size_bytes: recorded.contract_events_and_return_value_size,
            transaction_size_bytes,
            fee,
        };
        let (outcome, state_changes, after_digest) = match recorded.invoke_result {
            Ok(value) => {
                let after = apply_ledger_changes(&before, &recorded.ledger_changes)?;
                let after_digest = canonical_ledger_digest(&after)?;
                (
                    InvokeOutcome::Success(value),
                    state_diff(&before, &after)?,
                    after_digest,
                )
            }
            Err(error) => (failure_outcome(&error), Vec::new(), before_digest),
        };
        Ok(TransactionReceipt {
            call: call.clone(),
            authorization: recorded.auth,
            transaction_data,
            outcome,
            events: recorded
                .contract_events
                .into_iter()
                .map(|event| DetachedEvent {
                    event,
                    failed_call: false,
                })
                .collect(),
            diagnostics,
            state_changes,
            before_digest,
            after_digest,
            disposition: ReceiptDisposition::Previewed,
            resources,
        })
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
        if restored
            .iter()
            .any(|index| *index as usize >= footprint.read_write.len())
        {
            return Err(TransactionError::InvalidTransactionData);
        }
        self.check_source_account(&call.source)?;

        let counts = footprint_counts(footprint, restored);
        let transaction_size_bytes = transaction_size(call, &transaction.authorization, data)?;
        let key_sizes = footprint
            .read_only
            .iter()
            .chain(footprint.read_write.iter())
            .filter(|key| matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)))
            .map(|key| encode(key).map(|bytes| bytes.len() as u64))
            .collect::<Result<Vec<_>, _>>()?;
        for (resource, limit, used) in [
            (
                TransactionResource::Instructions,
                limits.instructions,
                u64::from(resources.instructions),
            ),
            (
                TransactionResource::DiskReadBytes,
                u64::from(limits.disk_read_bytes),
                u64::from(resources.disk_read_bytes),
            ),
            (
                TransactionResource::WriteBytes,
                u64::from(limits.write_bytes),
                u64::from(resources.write_bytes),
            ),
            (
                TransactionResource::FootprintEntries,
                u64::from(limits.footprint_entries),
                u64::from(counts.keys),
            ),
            (
                TransactionResource::DiskReadEntries,
                u64::from(limits.disk_read_entries),
                u64::from(counts.disk_reads),
            ),
            (
                TransactionResource::WriteEntries,
                u64::from(limits.write_entries),
                u64::from(counts.writes),
            ),
            (
                TransactionResource::TransactionSizeBytes,
                u64::from(limits.transaction_size_bytes),
                u64::from(transaction_size_bytes),
            ),
            (
                TransactionResource::LedgerKeySizeBytes,
                u64::from(limits.ledger_key_size_bytes),
                key_sizes.iter().copied().max().unwrap_or(0),
            ),
        ] {
            if used > limit {
                return Err(TransactionError::NetworkLimitExceeded {
                    resource,
                    limit,
                    used,
                });
            }
        }
        let ledger = self.ledger_info();
        let non_refundable = config
            .resource_fee(
                resources,
                counts,
                transaction_size_bytes,
                0,
                &[],
                ledger.sequence_number,
            )
            .non_refundable;
        if data.resource_fee < non_refundable {
            return Err(TransactionError::InsufficientResourceFee {
                declared: data.resource_fee,
                required: non_refundable,
            });
        }

        let (entries, disk_read_bytes, cached_code) =
            self.footprint_entries(call, footprint, restored)?;
        if disk_read_bytes > resources.disk_read_bytes {
            return Err(TransactionError::DeclaredResourceExceeded {
                resource: TransactionResource::DiskReadBytes,
                declared: u64::from(resources.disk_read_bytes),
                used: u64::from(disk_read_bytes),
            });
        }

        let before = self.ledger_snapshot();
        let before_digest = canonical_ledger_digest(&before)?;
        let budget = config.budget(u64::from(resources.instructions))?;
        let module_cache = module_cache(&ledger, &cached_code);
        let mut diagnostics = Vec::new();
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
            ledger.clone(),
            entries.into_iter(),
            self.prng_seed(call)?.to_vec(),
            &mut diagnostics,
            None,
            module_cache,
        );
        let diagnostics = detached_diagnostics(diagnostics);
        let mut report = ResourceReport {
            instructions: budget.get_cpu_insns_consumed().unwrap_or(0),
            memory_bytes: budget.get_mem_bytes_consumed().unwrap_or(0),
            footprint_entries: counts.keys,
            disk_read_entries: counts.disk_reads,
            disk_read_bytes,
            write_entries: counts.writes,
            write_bytes: 0,
            contract_events_size_bytes: 0,
            transaction_size_bytes,
            fee: ResourceFee {
                non_refundable,
                refundable: 0,
                rent: 0,
            },
        };
        let rejected = |outcome, report| Executed {
            receipt: TransactionReceipt {
                call: call.clone(),
                authorization: transaction.authorization.clone(),
                transaction_data: data.clone(),
                outcome,
                events: Vec::new(),
                diagnostics: diagnostics.clone(),
                state_changes: Vec::new(),
                before_digest,
                after_digest: before_digest,
                disposition: ReceiptDisposition::Rejected,
                resources: report,
            },
            after: None,
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => return Ok(rejected(failure_outcome(&error), report)),
        };
        let return_value = match result.encoded_invoke_result {
            Ok(encoded) => encoded,
            Err(error) => return Ok(rejected(failure_outcome(&error), report)),
        };

        let write_bytes: usize = result
            .ledger_changes
            .iter()
            .filter(|change| !change.read_only)
            .filter_map(|change| change.encoded_new_value.as_ref())
            .map(Vec::len)
            .sum();
        report.write_bytes = u32::try_from(write_bytes).unwrap_or(u32::MAX);
        if report.write_bytes > resources.write_bytes {
            return Err(TransactionError::DeclaredResourceExceeded {
                resource: TransactionResource::WriteBytes,
                declared: u64::from(resources.write_bytes),
                used: u64::from(report.write_bytes),
            });
        }
        let events_size = result
            .encoded_contract_events
            .iter()
            .map(Vec::len)
            .sum::<usize>()
            + return_value.len();
        report.contract_events_size_bytes = u32::try_from(events_size).unwrap_or(u32::MAX);
        if report.contract_events_size_bytes > limits.contract_events_size_bytes {
            return Err(TransactionError::NetworkLimitExceeded {
                resource: TransactionResource::ContractEventsSizeBytes,
                limit: u64::from(limits.contract_events_size_bytes),
                used: u64::from(report.contract_events_size_bytes),
            });
        }
        check_written_entry_sizes(&result.ledger_changes, &limits)?;
        report.fee = config.resource_fee(
            resources,
            counts,
            transaction_size_bytes,
            report.contract_events_size_bytes,
            &extract_rent_changes(&result.ledger_changes),
            ledger.sequence_number,
        );
        if data.resource_fee < report.fee.total() {
            return Err(TransactionError::InsufficientResourceFee {
                declared: data.resource_fee,
                required: report.fee.total(),
            });
        }

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
        let after = apply_ledger_changes(&before, &result.ledger_changes)?;
        let after_digest = canonical_ledger_digest(&after)?;
        let state_changes = state_diff(&before, &after)?;
        Ok(Executed {
            receipt: TransactionReceipt {
                call: call.clone(),
                authorization: transaction.authorization.clone(),
                transaction_data: data.clone(),
                outcome: InvokeOutcome::Success(value),
                events,
                diagnostics,
                state_changes,
                before_digest,
                after_digest,
                disposition: ReceiptDisposition::Previewed,
                resources: report,
            },
            after: Some(after),
        })
    }

    fn check_source_account(&self, source: &AccountId) -> Result<(), TransactionError> {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: source.clone(),
        });
        let Some(LookupState::Present(entry, _)) = self.coverage.get(&key_id(&key)?) else {
            return Err(TransactionError::SourceAccountMissing);
        };
        let LedgerEntryData::Account(account) = &entry.data else {
            return Err(TransactionError::SourceAccountMissing);
        };
        let [master, _, medium, _] = account.thresholds.0;
        if master == 0 || master < medium {
            return Err(TransactionError::SourceAccountNotAuthorized);
        }
        Ok(())
    }

    /// Encoded footprint entries in Host order, disk-read bytes, and the
    /// WASM to cache.
    #[allow(clippy::type_complexity)]
    fn footprint_entries(
        &self,
        call: &TransactionCall,
        footprint: &LedgerFootprint,
        restored: &[u32],
    ) -> Result<
        (
            Vec<(Option<Vec<u8>>, Option<TtlLedgerEntryMeta>)>,
            u32,
            Vec<Vec<u8>>,
        ),
        TransactionError,
    > {
        let rent_budget = Budget::default();
        let (local_contracts, local_code_hashes) = self.local_knowledge(call)?;
        let mut entries = Vec::new();
        let mut unknown = BTreeSet::new();
        let mut disk_read_bytes = 0_u32;
        let mut cached_code = Vec::new();
        let keys = footprint.read_only.iter().map(|key| (key, false)).chain(
            footprint.read_write.iter().enumerate().map(|(index, key)| {
                (
                    key,
                    u32::try_from(index).is_ok_and(|index| restored.contains(&index)),
                )
            }),
        );
        for (key, is_restored) in keys {
            let id = key_id(key)?;
            match self.coverage.get(&id) {
                Some(LookupState::Present(entry, live_until)) => {
                    let encoded = encode(entry.as_ref())?;
                    let size = u32::try_from(encoded.len()).unwrap_or(u32::MAX);
                    let ttl = match &entry.data {
                        LedgerEntryData::ContractData(_) | LedgerEntryData::ContractCode(_) => {
                            let live_until_ledger =
                                live_until.ok_or(StrictForkError::HostInspection)?;
                            Some(TtlLedgerEntryMeta {
                                live_until_ledger,
                                entry_size_for_rent: entry_size_for_rent(&rent_budget, entry, size)
                                    .map_err(|_| StrictForkError::HostInspection)?,
                            })
                        }
                        _ => None,
                    };
                    if ttl.is_none() || is_restored {
                        disk_read_bytes = disk_read_bytes.saturating_add(size);
                    }
                    if let LedgerEntryData::ContractCode(code) = &entry.data {
                        if !is_restored {
                            cached_code.push(code.code.to_vec());
                        }
                    }
                    entries.push((Some(encoded), ttl));
                }
                Some(LookupState::Absent) => entries.push((None, None)),
                None if is_local_key(key, &local_contracts, &local_code_hashes, false) => {
                    entries.push((None, None));
                }
                None => {
                    unknown.insert(id);
                }
            }
        }
        if !unknown.is_empty() {
            return Err(StrictForkError::UnknownLedgerKeys {
                count: unknown.len(),
                keys: unknown.into_iter().collect(),
            }
            .into());
        }
        Ok((entries, disk_read_bytes, cached_code))
    }
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

fn restored_indices(data: &SorobanTransactionData) -> &[u32] {
    match &data.ext {
        SorobanTransactionDataExt::V0 => &[],
        SorobanTransactionDataExt::V1(ext) => ext.archived_soroban_entries.as_slice(),
    }
}

fn footprint_counts(footprint: &LedgerFootprint, restored: &[u32]) -> FootprintCounts {
    let classic = footprint
        .read_only
        .iter()
        .chain(footprint.read_write.iter())
        .filter(|key| !matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)))
        .count();
    let to_u32 = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    FootprintCounts {
        keys: to_u32(footprint.read_only.len() + footprint.read_write.len()),
        disk_reads: to_u32(classic + restored.len()),
        writes: to_u32(footprint.read_write.len()),
    }
}

fn check_written_entry_sizes(
    changes: &[LedgerEntryChange],
    limits: &TransactionLimits,
) -> Result<(), TransactionError> {
    for change in changes.iter().filter(|change| !change.read_only) {
        let Some(encoded) = &change.encoded_new_value else {
            continue;
        };
        let entry = LedgerEntry::from_xdr(encoded, Limits::none())
            .map_err(|_| StrictForkError::HostInspection)?;
        let (resource, limit, used) = match &entry.data {
            LedgerEntryData::ContractData(_) => (
                TransactionResource::LedgerEntrySizeBytes,
                limits.ledger_entry_size_bytes,
                encoded.len(),
            ),
            LedgerEntryData::ContractCode(code) => (
                TransactionResource::ContractCodeSizeBytes,
                limits.contract_code_size_bytes,
                code.code.len(),
            ),
            _ => continue,
        };
        let used = used as u64;
        if used > u64::from(limit) {
            return Err(TransactionError::NetworkLimitExceeded {
                resource,
                limit: u64::from(limit),
                used,
            });
        }
    }
    Ok(())
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

fn transaction_size(
    call: &TransactionCall,
    authorization: &[SorobanAuthorizationEntry],
    data: &SorobanTransactionData,
) -> Result<u32, TransactionError> {
    let AccountId(PublicKey::PublicKeyTypeEd25519(key)) = &call.source;
    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: Transaction {
            source_account: MuxedAccount::Ed25519(key.clone()),
            fee: u32::MAX,
            seq_num: SequenceNumber(0),
            cond: Preconditions::None,
            memo: Memo::None,
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
        signatures: vec![DecoratedSignature {
            hint: SignatureHint([0; 4]),
            signature: Signature(vec![0; 64].try_into().map_err(|_| StrictForkError::Xdr)?),
        }]
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
                read_only: Vec::new().try_into().unwrap_or_default(),
                read_write: Vec::new().try_into().unwrap_or_default(),
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

fn xdr_vec<T>(values: Vec<T>) -> Result<soroban_env_host::xdr::VecM<T>, StrictForkError> {
    values
        .try_into()
        .map_err(|_| StrictForkError::InvalidInvocationXdr)
}

fn encode<T: WriteXdr>(value: &T) -> Result<Vec<u8>, StrictForkError> {
    value
        .to_xdr(Limits::none())
        .map_err(|_| StrictForkError::Xdr)
}
