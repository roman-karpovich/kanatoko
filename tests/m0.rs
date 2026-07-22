use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use kanatoko::{FixtureError, Fork, FrozenFixture, RuntimeError};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    BytesM, ContractCodeEntry, ContractCodeEntryExt, Hash, LedgerEntry, LedgerEntryData,
    LedgerEntryExt, LedgerKey, LedgerKeyContractCode, ScAddress,
};
use soroban_ledger_snapshot::LedgerSnapshot;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    Address, IntoVal, TryFromVal,
};

mod stateful {
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
        sha256 = "6f6f469798b686cc485ad207f32e3f77009c4b69ab2437d9bdca97f149b54ba8",
    );
}

const NETWORK_PASSPHRASE: &str = "Standalone Network ; February 2017";
const STATEFUL_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_stateful_fixture.wasm");
const STATEFUL_WASM_SHA256: [u8; 32] = [
    0x6f, 0x6f, 0x46, 0x97, 0x98, 0xb6, 0x86, 0xcc, 0x48, 0x5a, 0xd2, 0x07, 0xf3, 0x2e, 0x3f, 0x77,
    0x00, 0x9c, 0x4b, 0x69, 0xab, 0x24, 0x37, 0xd9, 0xbd, 0xca, 0x97, 0xf1, 0x49, 0xb5, 0x4b, 0xa8,
];

fn network_id(passphrase: &str) -> [u8; 32] {
    Sha256::digest(passphrase.as_bytes()).into()
}

fn empty_snapshot() -> LedgerSnapshot {
    LedgerSnapshot {
        protocol_version: 27,
        sequence_number: 1_000,
        timestamp: 1_721_600_000,
        network_id: network_id(NETWORK_PASSPHRASE),
        base_reserve: 5_000_000,
        min_persistent_entry_ttl: 4_096,
        min_temp_entry_ttl: 16,
        max_entry_ttl: 6_312_000,
        ledger_entries: vec![],
    }
}

fn code_entry(byte: u8) -> (Box<LedgerKey>, (Box<LedgerEntry>, Option<u32>)) {
    let hash = Hash([byte; 32]);
    let entry = LedgerEntry {
        last_modified_ledger_seq: 999,
        data: LedgerEntryData::ContractCode(ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: hash.clone(),
            code: BytesM::try_from(vec![byte]).unwrap(),
        }),
        ext: LedgerEntryExt::V0,
    };
    (
        Box::new(LedgerKey::ContractCode(LedgerKeyContractCode { hash })),
        (Box::new(entry), Some(10_000)),
    )
}

fn fixture() -> FrozenFixture {
    FrozenFixture::from_snapshot(empty_snapshot(), NETWORK_PASSPHRASE).unwrap()
}

fn registered_fork(initial: i64) -> (Fork, Address) {
    let fork = Fork::from_fixture(&fixture());
    let contract = fork
        .register_wasm(STATEFUL_WASM, STATEFUL_WASM_SHA256, (initial,))
        .unwrap();
    (fork, contract)
}

#[test]
fn rejects_protocol_mismatch() {
    let mut snapshot = empty_snapshot();
    snapshot.protocol_version = 26;

    let error = FrozenFixture::from_snapshot(snapshot, NETWORK_PASSPHRASE).unwrap_err();
    assert!(matches!(
        error,
        FixtureError::UnsupportedProtocol {
            found: 26,
            supported: 27
        }
    ));
}

#[test]
fn rejects_network_mismatch() {
    let snapshot = empty_snapshot();

    let error = FrozenFixture::from_snapshot(snapshot, "wrong network").unwrap_err();
    assert!(matches!(error, FixtureError::NetworkMismatch { .. }));
}

#[test]
fn missing_and_malformed_files_have_typed_errors() {
    let temp = TestDir::new("fixture-errors");
    let missing = temp.path().join("missing.json");
    let malformed = temp.path().join("malformed.json");
    fs::write(&malformed, b"{ definitely not a ledger snapshot").unwrap();

    assert!(matches!(
        FrozenFixture::from_file(&missing, NETWORK_PASSPHRASE).unwrap_err(),
        FixtureError::Read { .. }
    ));
    assert!(matches!(
        FrozenFixture::from_file(&malformed, NETWORK_PASSPHRASE).unwrap_err(),
        FixtureError::Parse { .. }
    ));
}

#[test]
fn valid_fixture_file_round_trips() {
    let temp = TestDir::new("valid-fixture");
    let path = temp.path().join("ledger.json");
    let snapshot = empty_snapshot();
    snapshot.write_file(&path).unwrap();

    let loaded = FrozenFixture::from_file(&path, NETWORK_PASSPHRASE).unwrap();

    assert_eq!(loaded.ledger_snapshot(), &snapshot);
}

#[test]
fn digest_is_order_independent_and_covers_ttl() {
    let mut first = empty_snapshot();
    first.ledger_entries = vec![code_entry(1), code_entry(2)];
    let first = FrozenFixture::from_snapshot(first, NETWORK_PASSPHRASE).unwrap();

    let mut reversed = empty_snapshot();
    reversed.ledger_entries = vec![code_entry(2), code_entry(1)];
    let reversed = FrozenFixture::from_snapshot(reversed, NETWORK_PASSPHRASE).unwrap();

    let mut changed_ttl = empty_snapshot();
    changed_ttl.ledger_entries = vec![code_entry(1), code_entry(2)];
    let (_, (_, live_until)) = &mut changed_ttl.ledger_entries[0];
    *live_until = Some(10_001);
    let changed_ttl = FrozenFixture::from_snapshot(changed_ttl, NETWORK_PASSPHRASE).unwrap();

    assert_eq!(first.ledger_digest(), reversed.ledger_digest());
    assert_ne!(first.ledger_digest(), changed_ttl.ledger_digest());
}

#[test]
fn canonical_digest_v1_has_a_fixed_vector() {
    let mut snapshot = empty_snapshot();
    snapshot.ledger_entries = vec![code_entry(2), code_entry(1)];

    assert_eq!(
        kanatoko::canonical_ledger_digest(&snapshot).unwrap(),
        [
            0x1a, 0x6f, 0x67, 0x28, 0x8e, 0x91, 0xd3, 0xe0, 0x90, 0x59, 0x21, 0x7e, 0xa6, 0x11,
            0xae, 0xa5, 0xcf, 0xd2, 0xe7, 0x4e, 0x55, 0x27, 0x7a, 0x7d, 0x8f, 0x6d, 0x75, 0x37,
            0x63, 0xc1, 0xec, 0x72,
        ],
    );
}

#[test]
fn canonical_digest_covers_every_metadata_field() {
    let baseline = empty_snapshot();
    let baseline_digest = kanatoko::canonical_ledger_digest(&baseline).unwrap();

    let changed_snapshots = [
        {
            let mut changed = baseline.clone();
            changed.protocol_version += 1;
            changed
        },
        {
            let mut changed = baseline.clone();
            changed.sequence_number += 1;
            changed
        },
        {
            let mut changed = baseline.clone();
            changed.timestamp += 1;
            changed
        },
        {
            let mut changed = baseline.clone();
            changed.network_id[0] ^= 1;
            changed
        },
        {
            let mut changed = baseline.clone();
            changed.base_reserve += 1;
            changed
        },
        {
            let mut changed = baseline.clone();
            changed.min_persistent_entry_ttl += 1;
            changed
        },
        {
            let mut changed = baseline.clone();
            changed.min_temp_entry_ttl += 1;
            changed
        },
        {
            let mut changed = baseline.clone();
            changed.max_entry_ttl += 1;
            changed
        },
    ];

    for changed in changed_snapshots {
        assert_ne!(
            kanatoko::canonical_ledger_digest(&changed).unwrap(),
            baseline_digest,
        );
    }
}

#[test]
fn duplicate_ledger_keys_are_rejected() {
    let mut snapshot = empty_snapshot();
    snapshot.ledger_entries = vec![code_entry(1), code_entry(1)];

    let error = FrozenFixture::from_snapshot(snapshot, NETWORK_PASSPHRASE).unwrap_err();
    assert!(matches!(error, FixtureError::DuplicateLedgerKey { .. }));
}

#[test]
fn ledger_key_entry_mismatches_are_rejected() {
    let mut snapshot = empty_snapshot();
    let (_, value) = code_entry(1);
    let (wrong_key, _) = code_entry(2);
    snapshot.ledger_entries = vec![(wrong_key, value)];

    let error = FrozenFixture::from_snapshot(snapshot, NETWORK_PASSPHRASE).unwrap_err();
    assert!(matches!(error, FixtureError::LedgerKeyMismatch { .. }));
}

#[test]
fn production_wasm_registration_and_invocation() {
    let actual_hash: [u8; 32] = Sha256::digest(STATEFUL_WASM).into();
    assert_eq!(actual_hash, STATEFUL_WASM_SHA256);

    let (fork, contract) = registered_fork(41);
    let client = stateful::Client::new(fork.env(), &contract);

    assert_eq!(client.get(), 41);
}

#[test]
fn wasm_hash_is_validated_before_registration() {
    let fixture = fixture();
    let fork = Fork::from_fixture(&fixture);
    let before = fork.ledger_digest().unwrap();

    let error = fork.register_wasm(STATEFUL_WASM, [0xff; 32], (0_i64,));

    assert!(matches!(error, Err(RuntimeError::WasmHashMismatch { .. })));
    assert_eq!(fork.ledger_digest().unwrap(), before);
}

#[test]
fn successful_calls_persist_into_following_calls() {
    let (fork, contract) = registered_fork(10);
    let client = stateful::Client::new(fork.env(), &contract);

    assert_eq!(client.increment(&2), 12);
    assert_eq!(client.increment(&5), 17);
    assert_eq!(client.get(), 17);
}

#[test]
fn failed_call_rolls_back_storage_and_marks_event_failed() {
    let (fork, contract) = registered_fork(10);
    let client = stateful::Client::new(fork.env(), &contract);
    let events_before = fork.env().to_snapshot().events.0.len();

    assert!(client.try_increment_then_fail(&7).is_err());
    let events = fork.env().to_snapshot().events.0;
    let failed_events = &events[events_before..];
    assert!(!failed_events.is_empty());
    assert!(failed_events.iter().all(|event| event.failed_call));
    assert_eq!(client.get(), 10);
}

#[test]
fn checkpoint_revert_restores_ledger_and_sdk_generators() {
    let (mut fork, contract) = registered_fork(10);
    let contract_xdr: ScAddress = (&contract).into();
    let checkpoint = fork.checkpoint();
    let checkpoint_digest = fork.ledger_digest().unwrap();

    let first_address = Address::generate(fork.env());
    let first_address_xdr: ScAddress = (&first_address).into();
    authorized_increment(&fork, &contract, &first_address, 3);
    let advanced_generators = fork.env().to_snapshot().generators;
    assert_ne!(fork.ledger_digest().unwrap(), checkpoint_digest);

    fork.revert(checkpoint).unwrap();

    let restored_contract = Address::try_from_val(fork.env(), &contract_xdr).unwrap();
    let restored_client = stateful::Client::new(fork.env(), &restored_contract);
    assert_eq!(restored_client.get(), 10);
    assert_eq!(fork.ledger_digest().unwrap(), checkpoint_digest);
    assert!(fork.env().to_snapshot().events.0.is_empty());

    let repeated_address = Address::generate(fork.env());
    let repeated_address_xdr: ScAddress = (&repeated_address).into();
    assert_eq!(repeated_address_xdr, first_address_xdr);
    authorized_increment(&fork, &restored_contract, &repeated_address, 3);
    assert_eq!(fork.env().to_snapshot().generators, advanced_generators);
}

#[test]
fn checkpoint_from_another_fork_is_rejected_without_mutation() {
    let fixture = fixture();
    let first = Fork::from_fixture(&fixture);
    let mut second = Fork::from_fixture(&fixture);
    let checkpoint = first.checkpoint();
    let before = second.env().to_snapshot();

    let error = second.revert(checkpoint).unwrap_err();

    assert!(matches!(error, RuntimeError::CheckpointMismatch));
    assert_eq!(second.env().to_snapshot(), before);
}

#[test]
fn independent_forks_do_not_share_mutations() {
    let fixture = fixture();
    let first = Fork::from_fixture(&fixture);
    let second = Fork::from_fixture(&fixture);
    let first_contract = first
        .register_wasm(STATEFUL_WASM, STATEFUL_WASM_SHA256, (10_i64,))
        .unwrap();
    let second_contract = second
        .register_wasm(STATEFUL_WASM, STATEFUL_WASM_SHA256, (10_i64,))
        .unwrap();
    let first_client = stateful::Client::new(first.env(), &first_contract);
    let second_client = stateful::Client::new(second.env(), &second_contract);

    first_client.increment(&9);

    assert_eq!(first_client.get(), 19);
    assert_eq!(second_client.get(), 10);
}

#[test]
fn no_auto_snapshot_artifacts() {
    const CHILD_ENV: &str = "KANATOKO_SNAPSHOT_ARTIFACT_CHILD";

    if std::env::var_os(CHILD_ENV).is_some() {
        let (mut fork, contract) = registered_fork(10);
        let contract_xdr: ScAddress = (&contract).into();
        let checkpoint = fork.checkpoint();
        let client = stateful::Client::new(fork.env(), &contract);
        assert_eq!(client.increment(&1), 11);

        fork.revert(checkpoint).unwrap();

        let restored_contract = Address::try_from_val(fork.env(), &contract_xdr).unwrap();
        let restored_client = stateful::Client::new(fork.env(), &restored_contract);
        assert_eq!(restored_client.increment(&2), 12);
        return;
    }

    let temp = TestDir::new("snapshot-artifacts");
    let output = Command::new(std::env::current_exe().unwrap())
        .current_dir(temp.path())
        .env(CHILD_ENV, "1")
        .arg("--exact")
        .arg("no_auto_snapshot_artifacts")
        .arg("--nocapture")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "child test failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!temp.path().join("test_snapshots").exists());
}

fn authorized_increment(fork: &Fork, contract: &Address, user: &Address, amount: i64) {
    let env = fork.env();
    let invocation = MockAuthInvoke {
        contract,
        fn_name: "authorized_increment",
        args: (user.clone(), amount).into_val(env),
        sub_invokes: &[],
    };
    let auth = MockAuth {
        address: user,
        invoke: &invocation,
    };
    let auths = [auth];
    let client = stateful::Client::new(env, contract).mock_auths(&auths);
    client.authorized_increment(user, &amount);
}

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("kanatoko-{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
