//! Transaction validity and resource checks, mirroring Stellar Core at
//! Protocol 30 (`TransactionFrame::checkSorobanResources`,
//! `commonValidPreSeqNum`, and `InvokeHostFunctionOpFrame` apply).

use std::collections::BTreeSet;

use soroban_env_host::{
    e2e_invoke::LedgerEntryChange,
    xdr::{
        AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext, AlphaNum12, AlphaNum4,
        ContractDataDurability, LedgerEntry, LedgerFootprint, LedgerKey, Limits, ReadXdr,
        SorobanTransactionData, SorobanTransactionDataExt, TimeBounds, TrustLineAsset, WriteXdr,
    },
};

use super::{
    FootprintCounts, InvalidTransaction, ResourceExcess, TransactionLimits, TransactionResource,
};

pub(crate) fn restored_indices(data: &SorobanTransactionData) -> &[u32] {
    match &data.ext {
        SorobanTransactionDataExt::V0 => &[],
        SorobanTransactionDataExt::V1(ext) => ext.archived_soroban_entries.as_slice(),
    }
}

/// Footprint keys, disk-read entries (classic keys plus restored entries),
/// and write entries, as Core counts them for limits and fees.
pub(crate) fn footprint_counts(footprint: &LedgerFootprint, restored: &[u32]) -> FootprintCounts {
    let classic = footprint
        .read_only
        .iter()
        .chain(footprint.read_write.iter())
        .filter(|key| !is_soroban_key(key))
        .count();
    let to_u32 = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    FootprintCounts {
        keys: to_u32(footprint.read_only.len() + footprint.read_write.len()),
        disk_reads: to_u32(classic + restored.len()),
        writes: to_u32(footprint.read_write.len()),
    }
}

pub(crate) fn is_soroban_key(key: &LedgerKey) -> bool {
    matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_))
}

pub(crate) fn is_persistent_key(key: &LedgerKey) -> bool {
    match key {
        LedgerKey::ContractCode(_) => true,
        LedgerKey::ContractData(data) => data.durability == ContractDataDurability::Persistent,
        _ => false,
    }
}

fn over(resource: TransactionResource, limit: u64, used: u64) -> Result<(), InvalidTransaction> {
    if used > limit {
        return Err(InvalidTransaction::NetworkLimitExceeded {
            resource,
            limit,
            used,
        });
    }
    Ok(())
}

/// Core's `checkSorobanResources` and footprint duplicate check: declared
/// resources and the footprint against the network's per-transaction limits.
pub(crate) fn check_soroban_resources(
    limits: &TransactionLimits,
    data: &SorobanTransactionData,
    transaction_size_bytes: u32,
) -> Result<(), InvalidTransaction> {
    let resources = &data.resources;
    let footprint = &resources.footprint;
    let restored = restored_indices(data);
    let counts = footprint_counts(footprint, restored);
    over(
        TransactionResource::Instructions,
        limits.instructions,
        u64::from(resources.instructions),
    )?;
    over(
        TransactionResource::DiskReadBytes,
        u64::from(limits.disk_read_bytes),
        u64::from(resources.disk_read_bytes),
    )?;
    over(
        TransactionResource::WriteBytes,
        u64::from(limits.write_bytes),
        u64::from(resources.write_bytes),
    )?;
    over(
        TransactionResource::FootprintEntries,
        u64::from(limits.footprint_entries),
        u64::from(counts.keys),
    )?;
    over(
        TransactionResource::DiskReadEntries,
        u64::from(limits.disk_read_entries),
        u64::from(counts.disk_reads),
    )?;
    over(
        TransactionResource::WriteEntries,
        u64::from(limits.write_entries),
        u64::from(counts.writes),
    )?;
    for key in footprint
        .read_only
        .iter()
        .chain(footprint.read_write.iter())
    {
        if !footprint_key_type_is_valid(key) {
            return Err(InvalidTransaction::UnsupportedFootprintKey(Box::new(
                key.clone(),
            )));
        }
        let size = key
            .to_xdr(Limits::none())
            .map_or(u64::MAX, |bytes| bytes.len() as u64);
        over(
            TransactionResource::LedgerKeySizeBytes,
            u64::from(limits.ledger_key_size_bytes),
            size,
        )?;
    }
    over(
        TransactionResource::TransactionSizeBytes,
        u64::from(limits.transaction_size_bytes),
        u64::from(transaction_size_bytes),
    )?;
    let mut previous = None;
    for index in restored {
        let in_bounds = (*index as usize) < footprint.read_write.len();
        if previous.is_some_and(|previous| *index <= previous)
            || !in_bounds
            || !is_persistent_key(&footprint.read_write[*index as usize])
        {
            return Err(InvalidTransaction::InvalidArchivedEntries);
        }
        previous = Some(*index);
    }
    let mut seen = BTreeSet::new();
    for key in footprint
        .read_only
        .iter()
        .chain(footprint.read_write.iter())
    {
        if !seen.insert(key) {
            return Err(InvalidTransaction::DuplicateFootprintKey(Box::new(
                key.clone(),
            )));
        }
    }
    Ok(())
}

/// Footprints may hold accounts, contract data and code, and trustlines of
/// a valid, non-native asset to an account other than its issuer.
fn footprint_key_type_is_valid(key: &LedgerKey) -> bool {
    match key {
        LedgerKey::Account(_) | LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => true,
        LedgerKey::Trustline(trustline) => match &trustline.asset {
            TrustLineAsset::Native => false,
            TrustLineAsset::CreditAlphanum4(AlphaNum4 { asset_code, issuer }) => {
                asset_code_is_valid(&asset_code.0, 1) && issuer != &trustline.account_id
            }
            TrustLineAsset::CreditAlphanum12(AlphaNum12 { asset_code, issuer }) => {
                asset_code_is_valid(&asset_code.0, 5) && issuer != &trustline.account_id
            }
            TrustLineAsset::PoolShare(_) => true,
        },
        _ => false,
    }
}

/// Core's `isAssetValid` code rule: at least `minimum` alphanumeric
/// characters, followed only by zero padding.
fn asset_code_is_valid(code: &[u8], minimum: usize) -> bool {
    let length = code
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(code.len());
    length >= minimum
        && code[..length].iter().all(u8::is_ascii_alphanumeric)
        && code[length..].iter().all(|byte| *byte == 0)
}

/// Core's `isTooEarly` and `isTooLate` against the applying ledger's close
/// time; a zero bound is open.
pub(crate) fn check_time_bounds(
    time_bounds: Option<&TimeBounds>,
    close_time: u64,
) -> Result<(), InvalidTransaction> {
    let Some(bounds) = time_bounds else {
        return Ok(());
    };
    if bounds.min_time.0 != 0 && bounds.min_time.0 > close_time {
        return Err(InvalidTransaction::TooEarly {
            min_time: bounds.min_time.0,
            close_time,
        });
    }
    if bounds.max_time.0 != 0 && bounds.max_time.0 < close_time {
        return Err(InvalidTransaction::TooLate {
            max_time: bounds.max_time.0,
            close_time,
        });
    }
    Ok(())
}

/// Core's `validateContractLedgerEntry` for one encoded entry, read or
/// written: contract code against the maximum contract size and contract data
/// against the maximum entry size, both on the encoded entry size.
pub(crate) fn check_entry_size(
    key: &LedgerKey,
    encoded_size: usize,
    limits: &TransactionLimits,
) -> Option<ResourceExcess> {
    let (resource, limit) = match key {
        LedgerKey::ContractCode(_) => (
            TransactionResource::ContractCodeSizeBytes,
            limits.contract_code_size_bytes,
        ),
        LedgerKey::ContractData(_) => (
            TransactionResource::LedgerEntrySizeBytes,
            limits.ledger_entry_size_bytes,
        ),
        _ => return None,
    };
    let used = encoded_size as u64;
    (used > u64::from(limit)).then_some(ResourceExcess {
        resource,
        limit: u64::from(limit),
        used,
    })
}

/// Entry-size checks of every written entry, then the declared write bytes.
pub(crate) fn check_writes(
    changes: &[LedgerEntryChange],
    limits: &TransactionLimits,
    declared_write_bytes: u32,
) -> Result<u32, ResourceExcess> {
    let mut written = 0_u64;
    for change in changes.iter().filter(|change| !change.read_only) {
        let Some(encoded) = &change.encoded_new_value else {
            continue;
        };
        let Ok(entry) = LedgerEntry::from_xdr(encoded, Limits::none()) else {
            continue;
        };
        if let Some(excess) = check_entry_size(&entry.to_key(), encoded.len(), limits) {
            return Err(excess);
        }
        written = written.saturating_add(encoded.len() as u64);
        if written > u64::from(declared_write_bytes) {
            return Err(ResourceExcess {
                resource: TransactionResource::WriteBytes,
                limit: u64::from(declared_write_bytes),
                used: written,
            });
        }
    }
    Ok(u32::try_from(written).unwrap_or(u32::MAX))
}

/// Contract events plus the return value against the network limit.
pub(crate) fn check_events(size: u64, limits: &TransactionLimits) -> Option<ResourceExcess> {
    (size > u64::from(limits.contract_events_size_bytes)).then_some(ResourceExcess {
        resource: TransactionResource::ContractEventsSizeBytes,
        limit: u64::from(limits.contract_events_size_bytes),
        used: size,
    })
}

/// Core's `getAvailableBalance`: balance minus the minimum balance and the
/// selling liabilities.
pub(crate) fn available_balance(account: &AccountEntry, base_reserve: u32) -> i64 {
    let (selling, sponsoring, sponsored) = match &account.ext {
        AccountEntryExt::V0 => (0, 0, 0),
        AccountEntryExt::V1(v1) => match &v1.ext {
            AccountEntryExtensionV1Ext::V0 => (v1.liabilities.selling, 0, 0),
            AccountEntryExtensionV1Ext::V2(v2) => (
                v1.liabilities.selling,
                i64::from(v2.num_sponsoring),
                i64::from(v2.num_sponsored),
            ),
        },
    };
    let entries = 2 + i64::from(account.num_sub_entries) + sponsoring - sponsored;
    let minimum = entries.saturating_mul(i64::from(base_reserve));
    account
        .balance
        .saturating_sub(minimum)
        .saturating_sub(selling)
}

#[cfg(test)]
mod tests {
    use soroban_env_host::xdr::{
        AccountId, AssetCode4, ContractCodeEntry, ContractCodeEntryExt, ContractDataEntry,
        ContractId, ExtensionPoint, Hash, LedgerEntryData, LedgerEntryExt, LedgerKeyAccount,
        LedgerKeyContractCode, LedgerKeyContractData, LedgerKeyTrustLine, PublicKey, ScAddress,
        ScVal, SorobanResources, SorobanResourcesExtV0, TimePoint, Uint256,
    };

    use super::*;

    fn limits() -> TransactionLimits {
        TransactionLimits {
            instructions: 1_000,
            memory_bytes: 1_000,
            footprint_entries: 4,
            disk_read_entries: 2,
            disk_read_bytes: 100,
            write_entries: 2,
            write_bytes: 100,
            contract_events_size_bytes: 50,
            transaction_size_bytes: 500,
            ledger_key_size_bytes: 200,
            ledger_entry_size_bytes: 300,
            contract_code_size_bytes: 400,
        }
    }

    fn account(byte: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32]))),
        })
    }

    fn data_key(byte: u8, durability: ContractDataDurability) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([byte; 32]))),
            key: ScVal::U32(u32::from(byte)),
            durability,
        })
    }

    fn data(read_only: Vec<LedgerKey>, read_write: Vec<LedgerKey>) -> SorobanTransactionData {
        SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: read_only.try_into().unwrap(),
                    read_write: read_write.try_into().unwrap(),
                },
                instructions: 1_000,
                disk_read_bytes: 100,
                write_bytes: 100,
            },
            resource_fee: 0,
        }
    }

    fn exceeded(result: Result<(), InvalidTransaction>) -> TransactionResource {
        match result {
            Err(InvalidTransaction::NetworkLimitExceeded { resource, .. }) => resource,
            other => panic!("expected an exceeded limit, got {other:?}"),
        }
    }

    #[test]
    fn declared_resources_pass_at_the_limit_and_fail_one_above() {
        let limits = limits();
        let base = data(vec![account(1)], vec![account(2)]);
        assert!(check_soroban_resources(&limits, &base, 500).is_ok());

        let mut instructions = base.clone();
        instructions.resources.instructions += 1;
        assert_eq!(
            exceeded(check_soroban_resources(&limits, &instructions, 500)),
            TransactionResource::Instructions
        );
        let mut read = base.clone();
        read.resources.disk_read_bytes += 1;
        assert_eq!(
            exceeded(check_soroban_resources(&limits, &read, 500)),
            TransactionResource::DiskReadBytes
        );
        let mut write = base.clone();
        write.resources.write_bytes += 1;
        assert_eq!(
            exceeded(check_soroban_resources(&limits, &write, 500)),
            TransactionResource::WriteBytes
        );
        assert_eq!(
            exceeded(check_soroban_resources(&limits, &base, 501)),
            TransactionResource::TransactionSizeBytes
        );
    }

    #[test]
    fn footprint_counts_pass_at_the_limit_and_fail_one_above() {
        let limits = limits();
        let persistent = ContractDataDurability::Persistent;
        // Four keys, two classic disk reads, two writes: all at the limit.
        let at_limit = data(
            vec![account(1), data_key(3, persistent)],
            vec![account(2), data_key(4, persistent)],
        );
        assert!(check_soroban_resources(&limits, &at_limit, 1).is_ok());

        let footprint = data(
            vec![account(1), data_key(3, persistent), data_key(5, persistent)],
            vec![account(2), data_key(4, persistent)],
        );
        assert_eq!(
            exceeded(check_soroban_resources(&limits, &footprint, 1)),
            TransactionResource::FootprintEntries
        );
        let disk_reads = data(vec![account(1), account(3)], vec![account(2)]);
        assert_eq!(
            exceeded(check_soroban_resources(&limits, &disk_reads, 1)),
            TransactionResource::DiskReadEntries
        );
        let writes = data(
            vec![],
            vec![
                data_key(3, persistent),
                data_key(4, persistent),
                data_key(5, persistent),
            ],
        );
        assert_eq!(
            exceeded(check_soroban_resources(&limits, &writes, 1)),
            TransactionResource::WriteEntries
        );

        // Restored entries count as disk reads.
        let mut restored = data(vec![account(1)], vec![data_key(4, persistent)]);
        restored.ext = SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
            archived_soroban_entries: vec![0].try_into().unwrap(),
        });
        assert!(check_soroban_resources(&limits, &restored, 1).is_ok());
        restored.resources.footprint.read_only = vec![account(1), account(3)].try_into().unwrap();
        assert_eq!(
            exceeded(check_soroban_resources(&limits, &restored, 1)),
            TransactionResource::DiskReadEntries
        );
    }

    #[test]
    fn every_footprint_key_is_size_checked() {
        let key = account(1);
        let size = u32::try_from(key.to_xdr(Limits::none()).unwrap().len()).unwrap();
        let mut limits = limits();
        limits.ledger_key_size_bytes = size;
        assert!(check_soroban_resources(&limits, &data(vec![key.clone()], vec![]), 1).is_ok());
        limits.ledger_key_size_bytes = size - 1;
        assert_eq!(
            exceeded(check_soroban_resources(
                &limits,
                &data(vec![key], vec![]),
                1
            )),
            TransactionResource::LedgerKeySizeBytes
        );
    }

    #[test]
    fn footprint_key_types_duplicates_and_archived_indices_are_validated() {
        let limits = limits();
        let config = LedgerKey::ConfigSetting(soroban_env_host::xdr::LedgerKeyConfigSetting {
            config_setting_id: soroban_env_host::xdr::ConfigSettingId::ContractMaxSizeBytes,
        });
        assert!(matches!(
            check_soroban_resources(&limits, &data(vec![config], vec![]), 1),
            Err(InvalidTransaction::UnsupportedFootprintKey(_))
        ));
        let holder = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1; 32])));
        let trustline = |asset| {
            LedgerKey::Trustline(LedgerKeyTrustLine {
                account_id: holder.clone(),
                asset,
            })
        };
        let usdc = |issuer: u8| {
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USDC"),
                issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([issuer; 32]))),
            })
        };
        assert!(
            check_soroban_resources(&limits, &data(vec![trustline(usdc(2))], vec![]), 1).is_ok()
        );
        for invalid in [
            TrustLineAsset::Native,
            usdc(1),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"U\0SD"),
                issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2; 32]))),
            }),
        ] {
            assert!(matches!(
                check_soroban_resources(&limits, &data(vec![trustline(invalid)], vec![]), 1),
                Err(InvalidTransaction::UnsupportedFootprintKey(_))
            ));
        }

        let persistent = data_key(3, ContractDataDurability::Persistent);
        assert!(matches!(
            check_soroban_resources(
                &limits,
                &data(vec![persistent.clone()], vec![persistent.clone()]),
                1
            ),
            Err(InvalidTransaction::DuplicateFootprintKey(_))
        ));

        let temporary = data_key(4, ContractDataDurability::Temporary);
        let code = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([5; 32]),
        });
        for indices in [vec![1, 0], vec![0, 0], vec![2], vec![1]] {
            let mut restored = data(vec![], vec![persistent.clone(), temporary.clone()]);
            restored.ext = SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: indices.clone().try_into().unwrap(),
            });
            assert!(
                matches!(
                    check_soroban_resources(&limits, &restored, 1),
                    Err(InvalidTransaction::InvalidArchivedEntries)
                ),
                "{indices:?}"
            );
        }
        let mut restored = data(vec![], vec![persistent, code]);
        restored.ext = SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
            archived_soroban_entries: vec![0, 1].try_into().unwrap(),
        });
        assert!(check_soroban_resources(&limits, &restored, 1).is_ok());
    }

    fn change(entry: &LedgerEntry, read_only: bool) -> LedgerEntryChange {
        LedgerEntryChange {
            read_only,
            encoded_key: entry.to_key().to_xdr(Limits::none()).unwrap(),
            encoded_new_value: Some(entry.to_xdr(Limits::none()).unwrap()),
            ..LedgerEntryChange::default()
        }
    }

    fn contract_data(size: usize) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash([1; 32]))),
                key: ScVal::U32(1),
                durability: ContractDataDurability::Persistent,
                val: ScVal::Bytes(vec![7; size].try_into().unwrap()),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn contract_code(size: usize) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractCode(ContractCodeEntry {
                ext: ContractCodeEntryExt::V0,
                hash: Hash([2; 32]),
                code: vec![0; size].try_into().unwrap(),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn written_entry_sizes_pass_at_the_limit_and_fail_one_above() {
        let data_entry = contract_data(16);
        let code_entry = contract_code(16);
        let data_size = data_entry.to_xdr(Limits::none()).unwrap().len();
        let code_size = code_entry.to_xdr(Limits::none()).unwrap().len();
        let mut limits = limits();
        limits.ledger_entry_size_bytes = u32::try_from(data_size).unwrap();
        limits.contract_code_size_bytes = u32::try_from(code_size).unwrap();
        let declared = u32::MAX;
        let changes = [change(&data_entry, false), change(&code_entry, false)];
        assert_eq!(
            check_writes(&changes, &limits, declared).unwrap() as usize,
            data_size + code_size
        );

        limits.ledger_entry_size_bytes -= 1;
        assert_eq!(
            check_writes(&changes, &limits, declared)
                .unwrap_err()
                .resource,
            TransactionResource::LedgerEntrySizeBytes
        );
        limits.ledger_entry_size_bytes += 1;
        limits.contract_code_size_bytes -= 1;
        assert_eq!(
            check_writes(&changes, &limits, declared)
                .unwrap_err()
                .resource,
            TransactionResource::ContractCodeSizeBytes
        );
        assert!(check_entry_size(&code_entry.to_key(), code_size, &limits).is_some());
        limits.contract_code_size_bytes += 1;
        assert!(check_entry_size(&code_entry.to_key(), code_size, &limits).is_none());

        // Declared write bytes: exact passes, one byte less fails.
        let total = u32::try_from(data_size + code_size).unwrap();
        assert!(check_writes(&changes, &limits, total).is_ok());
        assert_eq!(
            check_writes(&changes, &limits, total - 1)
                .unwrap_err()
                .resource,
            TransactionResource::WriteBytes
        );
        // Read-only changes never count as writes.
        assert_eq!(
            check_writes(&[change(&data_entry, true)], &limits, 0).unwrap(),
            0
        );
    }

    #[test]
    fn events_and_time_bounds() {
        let limits = limits();
        assert!(check_events(50, &limits).is_none());
        assert_eq!(
            check_events(51, &limits).unwrap().resource,
            TransactionResource::ContractEventsSizeBytes
        );
        let bounds = |min, max| TimeBounds {
            min_time: TimePoint(min),
            max_time: TimePoint(max),
        };
        assert!(check_time_bounds(Some(&bounds(0, 0)), 100).is_ok());
        assert!(check_time_bounds(Some(&bounds(100, 100)), 100).is_ok());
        assert!(matches!(
            check_time_bounds(Some(&bounds(101, 0)), 100),
            Err(InvalidTransaction::TooEarly { .. })
        ));
        assert!(matches!(
            check_time_bounds(Some(&bounds(0, 99)), 100),
            Err(InvalidTransaction::TooLate { .. })
        ));
        assert!(check_time_bounds(None, 100).is_ok());
    }
}
