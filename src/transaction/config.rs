//! Soroban network configuration read from captured `ConfigSetting` entries.

use std::{collections::BTreeSet, rc::Rc};

use soroban_env_host::{
    budget::Budget,
    fees::{
        compute_rent_fee, compute_rent_write_fee_per_1kb, compute_transaction_resource_fee,
        FeeConfiguration, LedgerEntryRentChange, RentFeeConfiguration, RentWriteFeeConfiguration,
        TransactionResources,
    },
    xdr::{
        ConfigSettingContractBandwidthV0, ConfigSettingContractComputeV0,
        ConfigSettingContractEventsV0, ConfigSettingContractHistoricalDataV0,
        ConfigSettingContractLedgerCostExtV0, ConfigSettingContractLedgerCostV0,
        ConfigSettingEntry, ConfigSettingId, ContractCostParams, LedgerEntry, LedgerEntryData,
        LedgerKey, LedgerKeyConfigSetting, Limits, SorobanResources, StateArchivalSettings,
        WriteXdr,
    },
};

use super::{FootprintCounts, TransactionError, TransactionLimits};

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
    ledger_target_close_time_milliseconds: u32,
    frozen_keys: BTreeSet<Vec<u8>>,
}

impl NetworkConfig {
    /// `ConfigSetting` entries a fixture must contain for transaction-faithful
    /// execution. [`crate::CaptureBuilder::capture_network_config`] and
    /// [`crate::AutoRunner::capture_network_config`] capture exactly these.
    pub const REQUIRED_SETTINGS: &'static [ConfigSettingId] = &[
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
        ConfigSettingId::ScpTiming,
        ConfigSettingId::FrozenLedgerKeys,
    ];

    pub(crate) fn from_lookup(
        mut lookup: impl FnMut(&LedgerKey) -> Option<Rc<LedgerEntry>>,
    ) -> Result<Self, TransactionError> {
        let mut settings = Vec::new();
        let mut missing = Vec::new();
        for id in Self::REQUIRED_SETTINGS {
            match lookup(&config_setting_key(*id))
                .as_deref()
                .map(|entry| &entry.data)
            {
                Some(LedgerEntryData::ConfigSetting(setting)) if setting.discriminant() == *id => {
                    settings.push(setting.clone());
                }
                _ => missing.push(*id),
            }
        }
        if !missing.is_empty() {
            return Err(TransactionError::MissingNetworkConfig { missing });
        }
        let mut found = Found::default();
        for setting in settings {
            found.insert(setting)?;
        }
        found.finish()
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
        // Integer average, as Stellar Core and RPC preflight compute it.
        let samples = self.live_state_size_window.len() as u64;
        let total = self
            .live_state_size_window
            .iter()
            .fold(0_u64, |total, sample| total.saturating_add(*sample));
        total.checked_div(samples).unwrap_or(0)
    }

    /// Seconds between two ledger closes on the fork: the network's target
    /// ledger close time, rounded up to whole seconds.
    #[must_use]
    pub fn ledger_close_seconds(&self) -> u64 {
        u64::from(self.ledger_target_close_time_milliseconds)
            .div_ceil(1000)
            .max(1)
    }

    /// Whether the network has frozen `key`; a transaction whose source
    /// account or footprint contains it is invalid.
    #[must_use]
    pub fn is_frozen(&self, key: &LedgerKey) -> bool {
        key.to_xdr(Limits::none())
            .is_ok_and(|encoded| self.frozen_keys.contains(&encoded))
    }

    /// Number of frozen ledger keys.
    #[must_use]
    pub fn frozen_key_count(&self) -> usize {
        self.frozen_keys.len()
    }

    pub(crate) fn tx_memory_limit(&self) -> u64 {
        u64::from(self.compute.tx_memory_limit)
    }

    pub(crate) fn budget(&self, instructions: u64) -> Result<Budget, TransactionError> {
        Budget::try_from_configs(
            instructions,
            self.tx_memory_limit(),
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

    /// `(non_refundable, events)` fees of `declared` resources, as Stellar
    /// Core computes them with the Host's fee function.
    pub(crate) fn resource_fees(
        &self,
        declared: &SorobanResources,
        counts: FootprintCounts,
        transaction_size_bytes: u32,
        contract_events_size_bytes: u32,
    ) -> (i64, i64) {
        compute_transaction_resource_fee(
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
        )
    }

    pub(crate) fn rent_fee(&self, changes: &[LedgerEntryRentChange], ledger_sequence: u32) -> i64 {
        compute_rent_fee(changes, &self.rent_fee_configuration(), ledger_sequence)
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
        .iter()
        .copied()
        .map(config_setting_key)
        .collect()
}

#[derive(Default)]
struct Found {
    contract_max_size_bytes: Option<u32>,
    compute: Option<ConfigSettingContractComputeV0>,
    ledger_cost: Option<ConfigSettingContractLedgerCostV0>,
    ledger_cost_ext: Option<ConfigSettingContractLedgerCostExtV0>,
    historical: Option<ConfigSettingContractHistoricalDataV0>,
    events: Option<ConfigSettingContractEventsV0>,
    bandwidth: Option<ConfigSettingContractBandwidthV0>,
    cpu_cost_params: Option<ContractCostParams>,
    memory_cost_params: Option<ContractCostParams>,
    data_key_size_bytes: Option<u32>,
    data_entry_size_bytes: Option<u32>,
    state_archival: Option<StateArchivalSettings>,
    live_state_size_window: Option<Vec<u64>>,
    ledger_target_close_time_milliseconds: Option<u32>,
    frozen_keys: Option<BTreeSet<Vec<u8>>>,
}

impl Found {
    fn insert(&mut self, setting: ConfigSettingEntry) -> Result<(), TransactionError> {
        match setting {
            ConfigSettingEntry::ContractMaxSizeBytes(value) => {
                self.contract_max_size_bytes = Some(value);
            }
            ConfigSettingEntry::ContractComputeV0(value) => self.compute = Some(value),
            ConfigSettingEntry::ContractLedgerCostV0(value) => self.ledger_cost = Some(value),
            ConfigSettingEntry::ContractLedgerCostExtV0(value) => {
                self.ledger_cost_ext = Some(value);
            }
            ConfigSettingEntry::ContractHistoricalDataV0(value) => self.historical = Some(value),
            ConfigSettingEntry::ContractEventsV0(value) => self.events = Some(value),
            ConfigSettingEntry::ContractBandwidthV0(value) => self.bandwidth = Some(value),
            ConfigSettingEntry::ContractCostParamsCpuInstructions(value) => {
                self.cpu_cost_params = Some(value);
            }
            ConfigSettingEntry::ContractCostParamsMemoryBytes(value) => {
                self.memory_cost_params = Some(value);
            }
            ConfigSettingEntry::ContractDataKeySizeBytes(value) => {
                self.data_key_size_bytes = Some(value);
            }
            ConfigSettingEntry::ContractDataEntrySizeBytes(value) => {
                self.data_entry_size_bytes = Some(value);
            }
            ConfigSettingEntry::StateArchival(value) => self.state_archival = Some(value),
            ConfigSettingEntry::LiveSorobanStateSizeWindow(value) => {
                if value.is_empty() {
                    return Err(TransactionError::InvalidNetworkConfig);
                }
                self.live_state_size_window = Some(value.to_vec());
            }
            ConfigSettingEntry::ScpTiming(value) => {
                self.ledger_target_close_time_milliseconds =
                    Some(value.ledger_target_close_time_milliseconds);
            }
            ConfigSettingEntry::FrozenLedgerKeys(value) => {
                self.frozen_keys = Some(value.keys.iter().map(|key| key.0.to_vec()).collect());
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<NetworkConfig, TransactionError> {
        let invalid = || TransactionError::InvalidNetworkConfig;
        Ok(NetworkConfig {
            contract_max_size_bytes: self.contract_max_size_bytes.ok_or_else(invalid)?,
            compute: self.compute.ok_or_else(invalid)?,
            ledger_cost: self.ledger_cost.ok_or_else(invalid)?,
            ledger_cost_ext: self.ledger_cost_ext.ok_or_else(invalid)?,
            historical: self.historical.ok_or_else(invalid)?,
            events: self.events.ok_or_else(invalid)?,
            bandwidth: self.bandwidth.ok_or_else(invalid)?,
            cpu_cost_params: self.cpu_cost_params.ok_or_else(invalid)?,
            memory_cost_params: self.memory_cost_params.ok_or_else(invalid)?,
            data_key_size_bytes: self.data_key_size_bytes.ok_or_else(invalid)?,
            data_entry_size_bytes: self.data_entry_size_bytes.ok_or_else(invalid)?,
            state_archival: self.state_archival.ok_or_else(invalid)?,
            live_state_size_window: self.live_state_size_window.ok_or_else(invalid)?,
            ledger_target_close_time_milliseconds: self
                .ledger_target_close_time_milliseconds
                .ok_or_else(invalid)?,
            frozen_keys: self.frozen_keys.ok_or_else(invalid)?,
        })
    }
}
