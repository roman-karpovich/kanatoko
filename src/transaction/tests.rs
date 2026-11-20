//! Unit tests with access to the fork's internals.

use std::collections::BTreeMap;

use serde::Deserialize;
use soroban_env_host::{
    budget::Budget,
    e2e_invoke::entry_size_for_rent,
    fees::LedgerEntryRentChange,
    xdr::{
        ConfigSettingEntry, ConfigSettingId, EncodedLedgerKey, FeeBumpTransactionInnerTx,
        FrozenLedgerKeys, LedgerEntryChange as MetaChange, LedgerEntryType, LedgerKeyConfigSetting,
        Limits, ReadXdr, SorobanTransactionMetaExt, TransactionEnvelope, TransactionMeta,
        TransactionResult, TtlEntry,
    },
};

use super::*;
use crate::{strict::contract_instance_key, CapturedFixture, ExecutionMode};

const CAPTURE: &str = "fixtures/mainnet/network-config-p29/auto-capture.json";
const GOLDEN: &str = "fixtures/mainnet/network-config-p29/fee-golden.json";
const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const STATEFUL_WASM: &[u8] = include_bytes!("../../fixtures/wasm/kanatoko_stateful_fixture.wasm");
const TEN_XLM: i64 = 100_000_000;

#[derive(Deserialize)]
struct Golden {
    transactions: Vec<GoldenTransaction>,
}

#[derive(Deserialize)]
struct GoldenTransaction {
    hash: String,
    ledger: u32,
    envelope_xdr: String,
    result_xdr: String,
    result_meta_xdr: String,
}

fn fork() -> StrictFork {
    CapturedFixture::from_file(CAPTURE, MAINNET_PASSPHRASE)
        .unwrap()
        .fork()
}

/// Rent changes of one Mainnet transaction, rebuilt from its metadata.
/// `None` when an entry changed size without a TTL change, whose live-until
/// ledger the metadata does not carry.
fn meta_rent_changes(changes: &[MetaChange]) -> Option<Vec<LedgerEntryRentChange>> {
    let budget = Budget::default();
    let size = |entry: &LedgerEntry| {
        let encoded = entry.to_xdr(Limits::none()).unwrap();
        entry_size_for_rent(&budget, entry, u32::try_from(encoded.len()).unwrap()).unwrap()
    };
    let mut ttl_before = BTreeMap::new();
    let mut ttl_after = BTreeMap::new();
    let mut entries: BTreeMap<Vec<u8>, (LedgerKey, Option<u32>, Option<u32>)> = BTreeMap::new();
    for change in changes {
        let (entry, after) = match change {
            MetaChange::State(entry) => (entry, false),
            MetaChange::Created(entry)
            | MetaChange::Updated(entry)
            | MetaChange::Restored(entry) => (entry, true),
            MetaChange::Removed(_) => continue,
        };
        if let LedgerEntryData::Ttl(TtlEntry {
            key_hash,
            live_until_ledger_seq,
        }) = &entry.data
        {
            let map = if after {
                &mut ttl_after
            } else {
                &mut ttl_before
            };
            map.insert(key_hash.0, *live_until_ledger_seq);
            continue;
        }
        let key = entry.to_key();
        if !is_soroban_key(&key) {
            continue;
        }
        let slot = entries
            .entry(key.to_xdr(Limits::none()).unwrap())
            .or_insert((key, None, None));
        if after {
            slot.2 = Some(size(entry));
        } else {
            slot.1 = Some(size(entry));
        }
    }
    let mut rent = Vec::new();
    for (encoded, (key, old_size, new_size)) in entries {
        let hash: [u8; 32] = Sha256::digest(&encoded).into();
        let Some(new_size) = new_size else { continue };
        let (old_live, new_live) = match (ttl_before.get(&hash), ttl_after.get(&hash)) {
            (Some(old), Some(new)) => (*old, *new),
            (None, Some(new)) => (0, *new),
            _ if old_size.is_some_and(|old| old >= new_size) => continue,
            _ => return None,
        };
        rent.push(LedgerEntryRentChange {
            is_persistent: validate::is_persistent_key(&key),
            is_code_entry: key.discriminant() == LedgerEntryType::ContractCode,
            old_size_bytes: old_size.unwrap_or(0),
            new_size_bytes: new_size,
            old_live_until_ledger: old_live,
            new_live_until_ledger: new_live,
        });
    }
    Some(rent)
}

/// Fee components of real Mainnet Soroban transactions, recomputed from
/// their envelopes and metadata with the captured configuration, equal what
/// the network charged.
#[test]
fn computed_fees_equal_mainnet_charges() {
    let fork = fork();
    let config = fork.network_config().unwrap();
    let golden: Golden = serde_json::from_slice(&std::fs::read(GOLDEN).unwrap()).unwrap();
    assert_eq!(golden.transactions.len(), 4);
    let mut rent_checked = 0;
    let mut address_v2 = 0;
    for transaction in golden.transactions {
        let envelope =
            TransactionEnvelope::from_xdr_base64(&transaction.envelope_xdr, Limits::none())
                .unwrap();
        // A fee bump pays for the inner transaction's size and counts as one
        // more operation for the inclusion fee.
        let (v1, outer_fee, operations) = match &envelope {
            TransactionEnvelope::Tx(v1) => (v1, i64::from(v1.tx.fee), 1),
            TransactionEnvelope::TxFeeBump(bump) => {
                let FeeBumpTransactionInnerTx::Tx(inner) = &bump.tx.inner_tx;
                (inner, bump.tx.fee, 2)
            }
            TransactionEnvelope::TxV0(_) => panic!("{} has no Soroban data", transaction.hash),
        };
        let size = TransactionEnvelope::Tx(v1.clone())
            .to_xdr(Limits::none())
            .unwrap()
            .len();
        let size = u32::try_from(size).unwrap();
        let TransactionExt::V1(data) = &v1.tx.ext else {
            panic!("{} is not a Soroban transaction", transaction.hash);
        };
        let OperationBody::InvokeHostFunction(operation) = &v1.tx.operations[0].body else {
            panic!("{} is not InvokeHostFunction", transaction.hash);
        };
        address_v2 += operation
            .auth
            .iter()
            .filter(|entry| matches!(entry.credentials, SorobanCredentials::AddressV2(_)))
            .count();
        let TransactionMeta::V4(meta) =
            TransactionMeta::from_xdr_base64(&transaction.result_meta_xdr, Limits::none()).unwrap()
        else {
            panic!("Protocol 29 metadata is V4");
        };
        let soroban = meta.soroban_meta.as_ref().unwrap();
        let SorobanTransactionMetaExt::V1(charged) = &soroban.ext else {
            panic!("fee metadata is present");
        };
        let result =
            TransactionResult::from_xdr_base64(&transaction.result_xdr, Limits::none()).unwrap();

        let counts = footprint_counts(&data.resources.footprint, restored_indices(data));
        let (non_refundable, _) = config.resource_fees(&data.resources, counts, size, 0);
        assert_eq!(
            non_refundable, charged.total_non_refundable_resource_fee_charged,
            "{}",
            transaction.hash
        );

        let events_size = encoded_events_size(
            meta.operations[0].events.iter(),
            soroban.return_value.as_ref(),
        )
        .unwrap();
        let (_, events_fee) = config.resource_fees(&data.resources, counts, size, events_size);
        assert_eq!(
            events_fee + charged.rent_fee_charged,
            charged.total_refundable_resource_fee_charged,
            "{}",
            transaction.hash
        );
        if charged.rent_fee_charged > 0 {
            let rent = meta_rent_changes(&meta.operations[0].changes)
                .expect("the rent-paying transaction records its TTL changes");
            assert_eq!(
                config.rent_fee(&rent, transaction.ledger),
                charged.rent_fee_charged,
                "{}",
                transaction.hash
            );
            rent_checked += 1;
        }

        // Outside surge pricing the inclusion fee charged is the base fee.
        let inclusion_bid = outer_fee - data.resource_fee;
        assert_eq!(
            result.fee_charged
                - charged.total_non_refundable_resource_fee_charged
                - charged.total_refundable_resource_fee_charged,
            inclusion_bid.min(BASE_FEE * operations),
            "{}",
            transaction.hash
        );
    }
    assert_eq!(rent_checked, 2);
    assert!(address_v2 > 0, "Mainnet accepts AddressV2 credentials");
}

fn value_key(contract: &ScAddress) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: contract.clone(),
        key: ScVal::Symbol(ScSymbol("value".try_into().unwrap())),
        durability: ContractDataDurability::Persistent,
    })
}

/// A fork with the stateful candidate whose `value` entry is archived.
fn archived_fork() -> (StrictFork, Ed25519Signer, ScAddress) {
    let mut fork = fork();
    let user = Ed25519Signer::from_label("user");
    fork.create_local_account(&user, TEN_XLM).unwrap();
    let candidate = ScAddress::Contract(ContractId(Hash([0x5b; 32])));
    fork.register_candidate(
        candidate.clone(),
        STATEFUL_WASM,
        Sha256::digest(STATEFUL_WASM).into(),
        vec![ScVal::I64(41)],
    )
    .unwrap();
    // The SDK registration gives the instance a TTL of one ledger; keep it
    // live so that only `value` is archived.
    let instance = contract_instance_key(candidate.clone());
    let live = fork.ledger_entry(&instance).unwrap().unwrap();
    let live_until = fork.ledger_sequence() + 10_000;
    fork.override_entry(instance, Some((live.entry, Some(live_until))))
        .unwrap();
    let key = value_key(&candidate);
    let entry = fork.ledger_entry(&key).unwrap().unwrap();
    let archived_at = fork.ledger_sequence() - 1;
    fork.override_entry(key, Some((entry.entry, Some(archived_at))))
        .unwrap();
    (fork, user, candidate)
}

fn increment(fork: &StrictFork, user: &Ed25519Signer, contract: &ScAddress) -> PreparedTransaction {
    let call = TransactionCall::invoke(
        user,
        &InvokeRequest {
            contract: contract.clone(),
            function: ScSymbol("increment".try_into().unwrap()),
            args: vec![ScVal::I64(5)],
        },
    )
    .unwrap();
    fork.sign_transaction(&call, &[], 0, fork.ledger_sequence() + 100)
        .unwrap()
}

#[test]
fn archived_entries_are_restored_only_when_marked() {
    let (mut fork, user, candidate) = archived_fork();
    let prepared = increment(&fork, &user, &candidate);
    let restored = restored_indices(prepared.transaction_data()).to_vec();
    assert_eq!(restored.len(), 1);
    let footprint = &prepared.transaction_data().resources.footprint;
    assert_eq!(
        footprint.read_write[restored[0] as usize],
        value_key(&candidate)
    );

    // Without the restore marker the entry is archived, as on the network.
    let unmarked = prepared
        .clone()
        .with_transaction_data(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            ..prepared.transaction_data().clone()
        });
    let FailedApply::Rejected(receipt) = fork.expect_failed_apply(&unmarked).unwrap() else {
        panic!("an unmarked archived entry fails the operation");
    };
    assert_eq!(receipt.result, InvokeHostFunctionResultCode::EntryArchived);

    let receipt = fork.apply_transaction(&prepared).unwrap();
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));
    assert!(receipt.resources.fee.rent > 0, "restoring pays rent");
    assert_eq!(receipt.resources.disk_read_entries, 1, "the restored entry");
    let restored = fork.ledger_entry(&value_key(&candidate)).unwrap().unwrap();
    assert!(restored.live_until_ledger.unwrap() > fork.ledger_sequence());
}

#[test]
fn frozen_keys_are_refused_before_inclusion() {
    let (mut fork, user, candidate) = archived_fork();
    let prepared = increment(&fork, &user, &candidate);
    for key in [user.account_key(), value_key(&candidate)] {
        let entry = LedgerEntry {
            last_modified_ledger_seq: fork.ledger_sequence(),
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::FrozenLedgerKeys(
                FrozenLedgerKeys {
                    keys: vec![EncodedLedgerKey(
                        key.to_xdr(Limits::none()).unwrap().try_into().unwrap(),
                    )]
                    .try_into()
                    .unwrap(),
                },
            )),
            ext: LedgerEntryExt::V0,
        };
        fork.override_entry(
            LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
                config_setting_id: ConfigSettingId::FrozenLedgerKeys,
            }),
            Some((entry, None)),
        )
        .unwrap();
        assert_eq!(fork.network_config().unwrap().frozen_key_count(), 1);
        let FailedApply::Invalid(InvalidTransaction::FrozenKeyAccessed(frozen)) =
            fork.expect_failed_apply(&prepared).unwrap()
        else {
            panic!("a frozen key makes the transaction invalid");
        };
        assert_eq!(*frozen, key);
    }
}

#[test]
fn simulation_runs_at_the_last_closed_ledger_and_apply_in_the_next() {
    let (mut fork, user, candidate) = archived_fork();
    let ledger = fork.ledger_sequence();
    let prepared = increment(&fork, &user, &candidate);
    let simulation = fork.simulate_transaction(prepared.call()).unwrap();
    assert_eq!(simulation.receipt().ledger_sequence, ledger);
    let receipt = fork.apply_transaction(&prepared).unwrap();
    assert_eq!(receipt.ledger_sequence, ledger + 1);
    // SDK-style invoke sees the closed ledger and its state.
    let get = fork
        .invoke(
            InvokeRequest {
                contract: candidate,
                function: ScSymbol("get".try_into().unwrap()),
                args: vec![],
            },
            ExecutionMode::Preview,
            crate::AuthMode::Enforce(vec![]),
        )
        .unwrap();
    assert_eq!(get.outcome, InvokeOutcome::Success(ScVal::I64(46)));
}
