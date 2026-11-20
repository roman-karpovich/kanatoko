#![cfg(all(feature = "capture", kanatoko_protocol_29_fixtures))]

//! Transaction-faithful execution on a strict fork: signed authorization,
//! enforced footprints and network limits, and upload/create host functions.
//!
//! The fixture holds the Mainnet native XLM Stellar Asset Contract and the
//! network's Soroban `ConfigSetting` entries. Signing identities, their
//! accounts, and the stateful candidate contract are explicit local state.

use kanatoko::{
    mainnet, AuthMode, AutoRunner, CacheStatus, CapturedFixture, Ed25519Signer, ExecutionMode,
    FailedApply, InvokeOutcome, InvokeRequest, NetworkConfig, PreparedTransaction,
    ReceiptDisposition, ScenarioFork, StrictFork, StrictForkError, TransactionCall,
    TransactionError, TransactionResource,
};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    ContractDataDurability, ContractId, Hash, Int128Parts, LedgerKey, LedgerKeyContractData,
    ScAddress, ScError, ScErrorCode, ScSymbol, ScVal, SorobanAuthorizedFunction,
    SorobanCredentials,
};

const CAPTURE: &str = "fixtures/mainnet/network-config-p29/auto-capture.json";
const SOROSWAP_CAPTURE: &str = "fixtures/mainnet/soroswap-xlm-usdc/capture.json";
const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const STATEFUL_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_stateful_fixture.wasm");
const TEN_XLM: i64 = 100_000_000;

fn runner() -> AutoRunner {
    mainnet().cache(CAPTURE).capture_network_config()
}

/// Touches the native XLM contract so the capture holds its instance.
fn scenario(fork: &ScenarioFork<'_>) {
    let xlm = fork.contract(XLM);
    assert_eq!(fork.invoke::<u32>(&xlm, "decimals", ()), 7);
}

#[test]
#[ignore = "read-only Mainnet capture; run explicitly to refresh the fixture"]
fn refresh_mainnet_network_config_fixture() {
    let run = runner().refresh().run(scenario).unwrap();
    assert!(matches!(
        run.cache_status(),
        CacheStatus::Created | CacheStatus::Refreshed
    ));
    assert_eq!(run.fixture().provenance().protocol_version(), 29);
    run.fixture().fork().network_config().unwrap();
}

fn fixture() -> CapturedFixture {
    let run = runner().offline().run(scenario).unwrap();
    assert_eq!(run.cache_status(), CacheStatus::Hit);
    run.fixture().clone()
}

fn signer(byte: u8) -> Ed25519Signer {
    Ed25519Signer::from_seed([byte; 32])
}

/// A fork with funded local accounts for `signers` and the stateful candidate
/// installed locally with value 41.
fn fork_with(signers: &[&Ed25519Signer]) -> (StrictFork, ScAddress) {
    let mut fork = fixture().fork();
    for signer in signers {
        fork.create_local_account(&signer.account_id(), TEN_XLM)
            .unwrap();
    }
    let candidate = ScAddress::Contract(ContractId(Hash([0x5a; 32])));
    fork.register_candidate(
        candidate.clone(),
        STATEFUL_WASM,
        Sha256::digest(STATEFUL_WASM).into(),
        vec![ScVal::I64(41)],
    )
    .unwrap();
    (fork, candidate)
}

fn request(contract: &ScAddress, function: &str, args: Vec<ScVal>) -> InvokeRequest {
    InvokeRequest {
        contract: contract.clone(),
        function: ScSymbol(function.try_into().unwrap()),
        args,
    }
}

fn authorized_increment(contract: &ScAddress, user: &Ed25519Signer) -> InvokeRequest {
    request(
        contract,
        "authorized_increment",
        vec![ScVal::Address(user.address()), ScVal::I64(5)],
    )
}

fn value(fork: &mut StrictFork, contract: &ScAddress) -> ScVal {
    let receipt = fork
        .invoke(
            request(contract, "get", vec![]),
            ExecutionMode::Preview,
            AuthMode::Enforce(vec![]),
        )
        .unwrap();
    let InvokeOutcome::Success(value) = receipt.outcome else {
        panic!("get must succeed");
    };
    value
}

/// Simulates, signs with `signers`, and prepares `call`.
fn prepare(
    fork: &StrictFork,
    call: &TransactionCall,
    signers: &[&Ed25519Signer],
    nonce: i64,
) -> PreparedTransaction {
    let expiration = fork.ledger_sequence() + 100;
    fork.sign_transaction(call, signers, nonce, expiration)
        .unwrap()
}

fn rejected_with(failure: &FailedApply, expected: &ScError) {
    let FailedApply::Rejected(receipt) = failure else {
        panic!("expected a Host rejection, got {failure:?}");
    };
    assert_eq!(receipt.disposition, ReceiptDisposition::Rejected);
    assert_eq!(receipt.before_digest, receipt.after_digest);
    assert!(receipt.state_changes.is_empty());
    let InvokeOutcome::Failure { error, .. } = &receipt.outcome else {
        panic!("a rejected transaction must fail");
    };
    assert_eq!(error.as_ref(), Some(expected));
}

fn refused(failure: FailedApply) -> TransactionError {
    match failure {
        FailedApply::Refused(error) => *error,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

const AUTH_INVALID_ACTION: ScError = ScError::Auth(ScErrorCode::InvalidAction);

#[test]
fn network_config_is_captured_with_the_fixture() {
    let fixture = fixture();
    assert_eq!(fixture.provenance().protocol_version(), 29);
    let config = fixture.fork().network_config().unwrap();
    let limits = config.limits();
    assert!(limits.instructions >= 100_000_000);
    assert!(limits.memory_bytes > 0);
    assert!(limits.footprint_entries > 0);
    assert!(limits.transaction_size_bytes > 0);
    assert!(limits.contract_code_size_bytes > 0);
    assert!(config.average_soroban_state_size_bytes() > 0);
    assert_eq!(NetworkConfig::REQUIRED_SETTINGS.len(), 13);
}

#[test]
fn transactions_fail_closed_without_captured_network_config() {
    let mut fork = CapturedFixture::from_file(SOROSWAP_CAPTURE, MAINNET_PASSPHRASE)
        .unwrap()
        .fork();
    let source = signer(1);
    let call = TransactionCall::upload_wasm(&source, STATEFUL_WASM).unwrap();
    let missing = |error: TransactionError| match error {
        TransactionError::MissingNetworkConfig { missing } => missing,
        other => panic!("expected missing network config, got {other:?}"),
    };
    assert_eq!(
        missing(fork.network_config().unwrap_err()),
        NetworkConfig::REQUIRED_SETTINGS
    );
    assert_eq!(
        missing(fork.simulate_transaction(&call).unwrap_err()).len(),
        13
    );
    assert_eq!(
        missing(fork.prepare_transaction(&call, vec![]).unwrap_err()).len(),
        13
    );
    let before = fork.ledger_digest().unwrap();
    let account = fork
        .create_local_account(&source.account_id(), TEN_XLM)
        .unwrap();
    assert!(account.after.is_some());
    assert_ne!(fork.ledger_digest().unwrap(), before);
}

#[test]
fn signed_address_authorization_is_applied_with_an_enforced_footprint() {
    let relayer = signer(1);
    let user = signer(2);
    let (mut fork, candidate) = fork_with(&[&relayer, &user]);
    let call = TransactionCall::invoke(&relayer, &authorized_increment(&candidate, &user)).unwrap();

    let simulation = fork.simulate_transaction(&call).unwrap();
    assert_eq!(
        simulation.receipt().disposition,
        ReceiptDisposition::Previewed
    );
    assert_eq!(simulation.authorization().len(), 1);
    assert_eq!(simulation.authorization()[0].address, user.address());
    assert!(matches!(
        simulation.authorization()[0].invocation.function,
        SorobanAuthorizedFunction::ContractFn(_)
    ));

    let before = fork.ledger_digest().unwrap();
    let expiration = fork.ledger_sequence() + 100;
    let authorization = fork
        .authorize(&simulation, &[&user], 7, expiration)
        .unwrap();
    let prepared = fork.prepare_transaction(&call, authorization).unwrap();
    assert_eq!(prepared, prepare(&fork, &call, &[&user], 7));
    assert_eq!(fork.ledger_digest().unwrap(), before);
    let SorobanCredentials::AddressV2(credentials) = &prepared.authorization()[0].credentials
    else {
        panic!("a non-source address signs AddressV2 credentials");
    };
    assert_eq!(credentials.nonce, 7);
    let nonce_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: user.address(),
        key: ScVal::LedgerKeyNonce(soroban_env_host::xdr::ScNonceKey { nonce: 7 }),
        durability: ContractDataDurability::Temporary,
    });
    assert!(prepared.footprint_contains(&nonce_key));
    let data = prepared.transaction_data();
    assert!(data.resources.instructions > 0);
    assert!(data.resource_fee > 0);

    let receipt = fork.apply_transaction(&prepared).unwrap();
    assert_eq!(receipt.disposition, ReceiptDisposition::Committed);
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));
    assert_eq!(receipt.events.len(), 1);
    assert_ne!(receipt.before_digest, receipt.after_digest);
    assert_eq!(fork.ledger_digest().unwrap(), receipt.after_digest);
    assert!(receipt
        .state_changes
        .iter()
        .any(|change| change.key == nonce_key && change.after.is_some()));
    let resources = receipt.resources;
    assert!(resources.instructions > 0);
    assert!(resources.instructions <= u64::from(data.resources.instructions));
    assert!(resources.write_bytes > 0);
    assert!(resources.write_bytes <= data.resources.write_bytes);
    assert!(resources.transaction_size_bytes > 0);
    assert_eq!(resources.write_entries, 2);
    assert!(resources.fee.non_refundable > 0);
    assert!(resources.fee.refundable >= resources.fee.rent);
    assert!(resources.fee.total() <= data.resource_fee);
    assert_eq!(value(&mut fork, &candidate), ScVal::I64(46));

    // The consumed nonce now exists, so the same signed transaction replays
    // into a Host rejection and leaves the ledger unchanged.
    let replay = fork.expect_failed_apply(&prepared).unwrap();
    rejected_with(&replay, &ScError::Auth(ScErrorCode::ExistingValue));
    assert_eq!(value(&mut fork, &candidate), ScVal::I64(46));
}

#[test]
fn source_account_authorization_needs_no_signature() {
    let user = signer(2);
    let (mut fork, candidate) = fork_with(&[&user]);
    let call = TransactionCall::invoke(&user, &authorized_increment(&candidate, &user)).unwrap();
    let simulation = fork.simulate_transaction(&call).unwrap();
    assert_eq!(simulation.authorization()[0].address, user.address());

    let prepared = prepare(&fork, &call, &[], 0);
    assert_eq!(
        prepared.authorization()[0].credentials,
        SorobanCredentials::SourceAccount
    );
    let receipt = fork.apply_transaction(&prepared).unwrap();
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));

    // The same tree from another transaction source is not authorized.
    let relayer = signer(1);
    fork.create_local_account(&relayer.account_id(), TEN_XLM)
        .unwrap();
    let from_relayer =
        TransactionCall::invoke(&relayer, &authorized_increment(&candidate, &user)).unwrap();
    let error = fork
        .prepare_transaction(&from_relayer, prepared.authorization().to_vec())
        .unwrap_err();
    assert!(matches!(error, TransactionError::SimulationFailed(_)));
}

#[test]
fn signing_failures_are_rejected_atomically() {
    let relayer = signer(1);
    let user = signer(2);
    let stranger = signer(3);
    let (mut fork, candidate) = fork_with(&[&relayer, &user, &stranger]);
    let call = TransactionCall::invoke(&relayer, &authorized_increment(&candidate, &user)).unwrap();
    let prepared = prepare(&fork, &call, &[&user], 11);
    let simulation = fork.simulate_transaction(&call).unwrap();
    let tree = &simulation.authorization()[0];
    let expiration = fork.ledger_sequence() + 100;

    // Wrong signer: a funded key that is not a signer of the user's account.
    let wrong = fork
        .sign_authorization(tree, &stranger, 11, expiration)
        .unwrap();
    let failure = fork
        .expect_failed_apply(&prepared.clone().with_authorization(vec![wrong.clone()]))
        .unwrap();
    rejected_with(&failure, &AUTH_INVALID_ACTION);
    assert!(matches!(
        fork.prepare_transaction(&call, vec![wrong]),
        Err(TransactionError::SimulationFailed(_))
    ));

    // Tampered signature.
    let failure = fork
        .expect_failed_apply(&prepared.clone().with_tampered_signature(0))
        .unwrap();
    rejected_with(&failure, &AUTH_INVALID_ACTION);

    // Expired signature.
    let expired = fork
        .sign_authorization(tree, &user, 11, fork.ledger_sequence() - 1)
        .unwrap();
    let failure = fork
        .expect_failed_apply(&prepared.clone().with_authorization(vec![expired]))
        .unwrap();
    rejected_with(&failure, &ScError::Auth(ScErrorCode::InvalidInput));

    // Missing entry.
    let failure = fork
        .expect_failed_apply(&prepared.clone().without_authorization_entry(0))
        .unwrap();
    rejected_with(&failure, &AUTH_INVALID_ACTION);

    // No signer for the tree's address.
    assert!(matches!(
        fork.authorize(&simulation, &[&stranger], 11, expiration),
        Err(TransactionError::MissingSigner { address }) if address == user.address()
    ));

    // The untouched transaction still applies.
    let receipt = fork.apply_transaction(&prepared).unwrap();
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));
}

#[test]
fn footprint_and_limits_are_enforced_atomically() {
    let relayer = signer(1);
    let user = signer(2);
    let (mut fork, candidate) = fork_with(&[&relayer, &user]);
    let call = TransactionCall::invoke(&relayer, &authorized_increment(&candidate, &user)).unwrap();
    let prepared = prepare(&fork, &call, &[&user], 3);
    let limits = fork.network_config().unwrap().limits();

    // A key outside the footprint fails inside the Host, as on the network.
    let value_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: candidate.clone(),
        key: ScVal::Symbol(ScSymbol("value".try_into().unwrap())),
        durability: ContractDataDurability::Persistent,
    });
    assert!(prepared.footprint_contains(&value_key));
    let failure = fork
        .expect_failed_apply(&prepared.clone().without_footprint_key(&value_key))
        .unwrap();
    rejected_with(&failure, &ScError::Storage(ScErrorCode::ExceededLimit));

    // A declaration above the network limit is refused before execution.
    let mut data = prepared.transaction_data().clone();
    data.resources.instructions = u32::try_from(limits.instructions + 1).unwrap();
    let error = refused(
        fork.expect_failed_apply(&prepared.clone().with_transaction_data(data))
            .unwrap(),
    );
    assert!(matches!(
        error,
        TransactionError::NetworkLimitExceeded {
            resource: TransactionResource::Instructions,
            ..
        }
    ));

    // Too few declared instructions exhaust the Host budget.
    let mut data = prepared.transaction_data().clone();
    data.resources.instructions = 10_000;
    let failure = fork
        .expect_failed_apply(&prepared.clone().with_transaction_data(data))
        .unwrap();
    rejected_with(&failure, &ScError::Budget(ScErrorCode::ExceededLimit));

    // Writing more than declared is refused after execution.
    let mut data = prepared.transaction_data().clone();
    data.resources.write_bytes = 1;
    let error = refused(
        fork.expect_failed_apply(&prepared.clone().with_transaction_data(data))
            .unwrap(),
    );
    assert!(matches!(
        error,
        TransactionError::DeclaredResourceExceeded {
            resource: TransactionResource::WriteBytes,
            declared: 1,
            ..
        }
    ));

    // The declared resource fee must cover the computed fee.
    let mut data = prepared.transaction_data().clone();
    data.resource_fee = 1;
    let error = refused(
        fork.expect_failed_apply(&prepared.clone().with_transaction_data(data))
            .unwrap(),
    );
    assert!(matches!(
        error,
        TransactionError::InsufficientResourceFee { declared: 1, .. }
    ));

    // An uncaptured footprint key fails closed.
    let mut data = prepared.transaction_data().clone();
    let mut read_only = data.resources.footprint.read_only.to_vec();
    read_only.push(LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(ContractId(Hash([0x77; 32]))),
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    }));
    data.resources.footprint.read_only = read_only.try_into().unwrap();
    let error = refused(
        fork.expect_failed_apply(&prepared.clone().with_transaction_data(data))
            .unwrap(),
    );
    assert!(matches!(
        error,
        TransactionError::Strict(StrictForkError::UnknownLedgerKeys { count: 1, .. })
    ));

    // A transaction source without an account entry is invalid.
    let ghost = signer(9);
    let ghost_call =
        TransactionCall::invoke(&ghost, &authorized_increment(&candidate, &user)).unwrap();
    let ghost_prepared = fork
        .prepare_transaction(&ghost_call, prepared.authorization().to_vec())
        .unwrap();
    let error = refused(fork.expect_failed_apply(&ghost_prepared).unwrap());
    assert!(matches!(error, TransactionError::SourceAccountMissing));

    // A transaction that succeeds is reported, not committed.
    let before = fork.ledger_digest().unwrap();
    assert!(matches!(
        fork.expect_failed_apply(&prepared),
        Err(TransactionError::UnexpectedSuccess(receipt))
            if receipt.disposition == ReceiptDisposition::Previewed
    ));
    assert_eq!(fork.ledger_digest().unwrap(), before);
    assert_eq!(value(&mut fork, &candidate), ScVal::I64(41));
}

#[test]
fn upload_and_create_contract_v2_run_the_constructor_under_real_auth() {
    let deployer = signer(4);
    let mut fork = fixture().fork();
    fork.create_local_account(&deployer.account_id(), TEN_XLM)
        .unwrap();
    let wasm_hash: [u8; 32] = Sha256::digest(STATEFUL_WASM).into();

    let upload = TransactionCall::upload_wasm(&deployer, STATEFUL_WASM).unwrap();
    let uploaded = fork
        .apply_transaction(&prepare(&fork, &upload, &[], 0))
        .unwrap();
    assert_eq!(
        uploaded.outcome,
        InvokeOutcome::Success(ScVal::Bytes(wasm_hash.to_vec().try_into().unwrap()))
    );
    assert!(uploaded.resources.write_bytes as usize > STATEFUL_WASM.len());

    let salt = [0x21; 32];
    let create = TransactionCall::create_contract(
        &deployer,
        deployer.address(),
        salt,
        wasm_hash,
        vec![ScVal::I64(41)],
    )
    .unwrap();
    let simulation = fork.simulate_transaction(&create).unwrap();
    assert!(matches!(
        simulation.authorization()[0].invocation.function,
        SorobanAuthorizedFunction::CreateContractV2HostFn(_)
    ));
    let prepared = prepare(&fork, &create, &[], 0);
    let created = fork.apply_transaction(&prepared).unwrap();
    let expected = fork.contract_address(&deployer.address(), salt).unwrap();
    assert_eq!(
        created.outcome,
        InvokeOutcome::Success(ScVal::Address(expected.clone()))
    );
    // The constructor stored its argument.
    assert_eq!(value(&mut fork, &expected), ScVal::I64(41));

    // The created contract is usable by later transactions.
    let user = signer(2);
    fork.create_local_account(&user.account_id(), TEN_XLM)
        .unwrap();
    let increment =
        TransactionCall::invoke(&user, &authorized_increment(&expected, &user)).unwrap();
    let receipt = fork
        .apply_transaction(&prepare(&fork, &increment, &[], 0))
        .unwrap();
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));

    // The same deployer and salt cannot create over the existing instance.
    let failure = fork.expect_failed_apply(&prepared).unwrap();
    rejected_with(&failure, &ScError::Storage(ScErrorCode::ExistingValue));
}

#[test]
fn create_contract_v2_from_another_deployer_needs_its_signature() {
    let relayer = signer(1);
    let deployer = signer(4);
    let mut fork = fixture().fork();
    for account in [&relayer, &deployer] {
        fork.create_local_account(&account.account_id(), TEN_XLM)
            .unwrap();
    }
    let wasm_hash: [u8; 32] = Sha256::digest(STATEFUL_WASM).into();
    let upload = TransactionCall::upload_wasm(&relayer, STATEFUL_WASM).unwrap();
    fork.apply_transaction(&prepare(&fork, &upload, &[], 0))
        .unwrap();

    let salt = [0x22; 32];
    let create = TransactionCall::create_contract(
        &relayer,
        deployer.address(),
        salt,
        wasm_hash,
        vec![ScVal::I64(7)],
    )
    .unwrap();
    let prepared = prepare(&fork, &create, &[&deployer], 1);
    assert!(matches!(
        prepared.authorization()[0].credentials,
        SorobanCredentials::AddressV2(_)
    ));
    let failure = fork
        .expect_failed_apply(&prepared.clone().with_tampered_signature(0))
        .unwrap();
    rejected_with(&failure, &AUTH_INVALID_ACTION);

    let created = fork.apply_transaction(&prepared).unwrap();
    let expected = fork.contract_address(&deployer.address(), salt).unwrap();
    assert_eq!(
        created.outcome,
        InvokeOutcome::Success(ScVal::Address(expected.clone()))
    );
    assert_eq!(value(&mut fork, &expected), ScVal::I64(7));
}

#[test]
fn signed_native_xlm_transfer_on_the_captured_asset_contract() {
    let sender = signer(5);
    let receiver = signer(6);
    let mut fork = fixture().fork();
    for account in [&sender, &receiver] {
        fork.create_local_account(&account.account_id(), TEN_XLM)
            .unwrap();
    }
    let xlm: ScAddress = XLM.parse().unwrap();
    let transfer = request(
        &xlm,
        "transfer",
        vec![
            ScVal::Address(sender.address()),
            ScVal::Address(receiver.address()),
            ScVal::I128(Int128Parts { hi: 0, lo: 25 }),
        ],
    );
    let relayer_call = TransactionCall::invoke(&receiver, &transfer).unwrap();
    let receipt = fork
        .apply_transaction(&prepare(&fork, &relayer_call, &[&sender], 1))
        .unwrap();
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::Void));
    assert!(receipt.resources.disk_read_entries >= 2);
    assert!(receipt.resources.disk_read_bytes > 0);
    let balance = |fork: &mut StrictFork, account: &Ed25519Signer| {
        let receipt = fork
            .invoke(
                request(&xlm, "balance", vec![ScVal::Address(account.address())]),
                ExecutionMode::Preview,
                AuthMode::Enforce(vec![]),
            )
            .unwrap();
        receipt.outcome
    };
    assert_eq!(
        balance(&mut fork, &sender),
        InvokeOutcome::Success(ScVal::I128(Int128Parts {
            hi: 0,
            lo: (TEN_XLM - 25) as u64
        }))
    );
    assert_eq!(
        balance(&mut fork, &receiver),
        InvokeOutcome::Success(ScVal::I128(Int128Parts {
            hi: 0,
            lo: (TEN_XLM + 25) as u64
        }))
    );
}

#[test]
fn invoke_with_source_accepts_source_account_credentials() {
    let user = signer(2);
    let (mut fork, candidate) = fork_with(&[&user]);
    let recorded = fork
        .invoke(
            authorized_increment(&candidate, &user),
            ExecutionMode::Preview,
            AuthMode::Record,
        )
        .unwrap();
    let entry = recorded.authorization[0].source_account_authorization();

    // Without the user as the named source, its source-account credentials
    // do not authorize it.
    let before = fork.ledger_digest().unwrap();
    let rejected = fork
        .invoke(
            authorized_increment(&candidate, &user),
            ExecutionMode::Apply,
            AuthMode::Enforce(vec![entry.clone()]),
        )
        .unwrap();
    assert_eq!(rejected.disposition, ReceiptDisposition::Rejected);
    assert_eq!(fork.ledger_digest().unwrap(), before);

    let receipt = fork
        .invoke_with_source(
            &user,
            authorized_increment(&candidate, &user),
            ExecutionMode::Apply,
            AuthMode::Enforce(vec![entry]),
        )
        .unwrap();
    assert_eq!(receipt.disposition, ReceiptDisposition::Committed);
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));
}

#[test]
fn signed_entries_from_a_receipt_tree_apply_through_invoke() {
    let user = signer(2);
    let (mut fork, candidate) = fork_with(&[&user]);
    let recorded = fork
        .invoke(
            authorized_increment(&candidate, &user),
            ExecutionMode::Preview,
            AuthMode::Record,
        )
        .unwrap();
    let expiration = fork.ledger_sequence() + 10;
    let entry = fork
        .sign_authorization(&recorded.authorization[0], &user, 99, expiration)
        .unwrap();
    let receipt = fork
        .invoke(
            authorized_increment(&candidate, &user),
            ExecutionMode::Apply,
            AuthMode::Enforce(vec![entry]),
        )
        .unwrap();
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));
}

#[test]
fn local_overrides_are_explicit_and_strict() {
    let (mut fork, candidate) = fork_with(&[]);
    let key = ScVal::Symbol(ScSymbol("value".try_into().unwrap()));
    let change = fork
        .set_contract_data(
            &candidate,
            key.clone(),
            ContractDataDurability::Persistent,
            Some(ScVal::I64(1_000)),
        )
        .unwrap();
    assert!(change.before.is_some());
    assert_eq!(value(&mut fork, &candidate), ScVal::I64(1_000));
    let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: candidate.clone(),
        key: key.clone(),
        durability: ContractDataDurability::Persistent,
    });
    assert!(fork.ledger_entry(&ledger_key).unwrap().is_some());

    fork.set_contract_data(&candidate, key, ContractDataDurability::Persistent, None)
        .unwrap();
    assert!(fork.ledger_entry(&ledger_key).unwrap().is_none());
    assert_eq!(value(&mut fork, &candidate), ScVal::I64(0));

    let foreign = ScAddress::Contract(ContractId(Hash([0x66; 32])));
    assert!(matches!(
        fork.set_contract_data(
            &foreign,
            ScVal::U32(1),
            ContractDataDurability::Temporary,
            Some(ScVal::U32(2)),
        ),
        Err(StrictForkError::UnknownLedgerKeys { .. })
    ));

    let user = signer(2);
    fork.create_local_account(&user.account_id(), TEN_XLM)
        .unwrap();
    assert!(matches!(
        fork.create_local_account(&user.account_id(), TEN_XLM),
        Err(TransactionError::LocalAccountExists)
    ));
    assert!(matches!(
        fork.create_local_account(&signer(8).account_id(), 1),
        Err(TransactionError::LocalAccountBalanceTooLow { .. })
    ));
}

/// The README sample, replayed from the committed capture.
#[test]
fn readme_signed_transfer() -> Result<(), Box<dyn std::error::Error>> {
    let run = runner().offline().run(|fork| {
        let xlm = fork.contract(XLM);
        fork.invoke::<u32>(&xlm, "decimals", ());
    })?;
    let mut fork = run.fixture().fork();

    let sender = Ed25519Signer::from_seed([1; 32]);
    let relayer = Ed25519Signer::from_seed([2; 32]);
    fork.create_local_account(&sender.account_id(), 100_000_000)?;
    fork.create_local_account(&relayer.account_id(), 100_000_000)?;

    let transfer = InvokeRequest {
        contract: XLM.parse::<ScAddress>()?,
        function: ScSymbol("transfer".try_into()?),
        args: vec![
            ScVal::Address(sender.address()),
            ScVal::Address(relayer.address()),
            ScVal::I128(Int128Parts { hi: 0, lo: 25 }),
        ],
    };
    let call = TransactionCall::invoke(&relayer, &transfer)?;
    let simulation = fork.simulate_transaction(&call)?;
    let expiration = fork.ledger_sequence() + 100;
    let authorization = fork.authorize(&simulation, &[&sender], 1, expiration)?;
    let transaction = fork.prepare_transaction(&call, authorization)?;

    let receipt = fork.apply_transaction(&transaction)?;
    assert!(receipt.outcome.is_success());
    assert!(receipt.resources.instructions > 0);
    assert!(receipt.resources.fee.total() > 0);

    let failure = fork.expect_failed_apply(&transaction)?;
    rejected_with(&failure, &ScError::Auth(ScErrorCode::ExistingValue));
    Ok(())
}
