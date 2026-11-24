#![cfg(all(feature = "capture", kanatoko_protocol_30_fixtures))]

//! Transaction-faithful execution on a strict fork: signed authorization,
//! enforced footprints, network limits, fees, and upload/create.
//!
//! The fixture holds the Mainnet native XLM Stellar Asset Contract, the
//! network's Soroban `ConfigSetting` entries, and the confirmed absence of
//! every test signer's account, the stateful WASM, and the test deployments.
//! Signing accounts and the stateful candidate are explicit local state.

use kanatoko::{
    mainnet, AuthMode, AuthorizationFormat, AutoRunner, CacheStatus, CapturedFixture,
    Ed25519Signer, ExecutionMode, FailedApply, InvalidTransaction, InvokeOutcome, InvokeRequest,
    NetworkConfig, PreparedTransaction, ReceiptDisposition, ScenarioFork, StrictFork,
    StrictForkError, TransactionCall, TransactionError, TransactionReceipt, TransactionResource,
};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    ContractDataDurability, ContractId, Hash, Int128Parts, InvokeHostFunctionResultCode,
    LedgerEntryData, LedgerKey, LedgerKeyContractData, Memo, ScAddress, ScError, ScErrorCode,
    ScNonceKey, ScSymbol, ScVal, SorobanAuthorizationEntry, SorobanAuthorizedFunction,
    SorobanCredentials, SorobanResourcesExtV0, SorobanTransactionData, SorobanTransactionDataExt,
    TimeBounds, TimePoint,
};

const CAPTURE: &str = "fixtures/mainnet/network-config-p30/auto-capture.json";
const SOROSWAP_CAPTURE: &str = "fixtures/mainnet/soroswap-xlm-usdc/capture.json";
const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const STATEFUL_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_stateful_fixture.wasm");
const QUOTE_FORWARDER_WASM: &[u8] =
    include_bytes!("../fixtures/wasm/kanatoko_quote_forwarder.wasm");
/// Enough XLM for uploads, whose rent dominates their fee.
const FUNDING: i64 = 10_000_000_000;
/// `max_entry_ttl` of the captured `StateArchival` setting.
const MAX_ENTRY_TTL: u32 = 3_110_400;
/// Two Mainnet base reserves.
const MINIMUM_BALANCE: i64 = 10_000_000;

/// Test signers whose accounts the capture confirms absent on Mainnet.
const SIGNERS: &[&str] = &[
    "relayer", "user", "stranger", "deployer", "sender", "receiver", "spare", "poor",
];
/// `CreateContractV2` deployments by signer label, captured absent.
const DEPLOYMENTS: &[(&str, [u8; 32])] = &[("deployer", [0x21; 32]), ("deployer", [0x22; 32])];

fn signer(label: &str) -> Ed25519Signer {
    Ed25519Signer::from_label(label)
}

/// A well-known seed whose account exists on Mainnet.
fn existing_signer() -> Ed25519Signer {
    Ed25519Signer::from_seed([1; 32])
}

fn runner() -> AutoRunner {
    mainnet().cache(CAPTURE).capture_network_config()
}

fn scenario(fork: &ScenarioFork<'_>) {
    let xlm = fork.contract(XLM);
    assert_eq!(fork.invoke::<u32>(&xlm, "decimals", ()), 7);
    for label in SIGNERS {
        fork.capture_signer(&signer(label));
    }
    fork.capture_signer(&existing_signer());
    fork.capture_wasm(STATEFUL_WASM);
    for (deployer, salt) in DEPLOYMENTS {
        let _ = fork.capture_deployed_contract(&signer(deployer).address(), *salt);
    }
}

#[test]
#[ignore = "read-only Mainnet capture; run explicitly to refresh the fixture"]
fn refresh_mainnet_network_config_fixture() {
    let run = runner().refresh().run(scenario).unwrap();
    assert!(matches!(
        run.cache_status(),
        CacheStatus::Created | CacheStatus::Refreshed
    ));
    assert_eq!(run.fixture().provenance().protocol_version(), 30);
    run.fixture().fork().network_config().unwrap();
}

fn fixture() -> CapturedFixture {
    let run = runner().offline().run(scenario).unwrap();
    assert_eq!(run.cache_status(), CacheStatus::Hit);
    run.fixture().clone()
}

/// A fork with funded local accounts for `labels` and the stateful contract
/// uploaded and created with value 41 by the `deployer` signer.
fn fork_with(labels: &[&str]) -> (StrictFork, ScAddress) {
    let mut fork = fixture().fork();
    for label in labels.iter().chain(std::iter::once(&"deployer")) {
        if *label == "deployer"
            && fork
                .ledger_entry(&signer(label).account_key())
                .unwrap()
                .is_some()
        {
            continue;
        }
        fork.create_local_account(&signer(label), FUNDING).unwrap();
    }
    let deployer = signer("deployer");
    let upload = TransactionCall::upload_wasm(&deployer, STATEFUL_WASM).unwrap();
    fork.apply_transaction(&prepare(&fork, &upload, &[], 0))
        .unwrap();
    let create = TransactionCall::create_contract(
        &deployer,
        deployer.address(),
        DEPLOYMENTS[0].1,
        Sha256::digest(STATEFUL_WASM).into(),
        vec![ScVal::I64(41)],
    )
    .unwrap();
    let receipt = fork
        .apply_transaction(&prepare(&fork, &create, &[], 0))
        .unwrap();
    let InvokeOutcome::Success(ScVal::Address(contract)) = receipt.outcome else {
        panic!("CreateContractV2 returns the contract address");
    };
    (fork, contract)
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

fn account(fork: &StrictFork, signer: &Ed25519Signer) -> (i64, i64) {
    let entry = fork.ledger_entry(&signer.account_key()).unwrap().unwrap();
    let LedgerEntryData::Account(account) = entry.entry.data else {
        panic!("an account key holds an account");
    };
    (account.balance, account.seq_num.0)
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

fn edited(
    prepared: &PreparedTransaction,
    edit: impl FnOnce(&mut SorobanTransactionData),
) -> PreparedTransaction {
    let mut data = prepared.transaction_data().clone();
    edit(&mut data);
    prepared.clone().with_transaction_data(data)
}

/// An included, failed transaction: fee charged, nothing else changed.
fn rejected(failure: &FailedApply) -> &TransactionReceipt {
    let FailedApply::Rejected(receipt) = failure else {
        panic!("expected an included, failed transaction, got {failure:?}");
    };
    assert_eq!(receipt.disposition, ReceiptDisposition::Rejected);
    assert!(receipt.fee_charged > 0);
    assert!(receipt.events.is_empty());
    assert_eq!(receipt.state_changes.len(), 1, "only the source account");
    receipt
}

fn rejected_with(failure: &FailedApply, expected: &ScError) {
    let receipt = rejected(failure);
    assert_eq!(receipt.result, InvokeHostFunctionResultCode::Trapped);
    let InvokeOutcome::Failure { error, .. } = &receipt.outcome else {
        panic!("a rejected transaction must fail");
    };
    assert_eq!(error.as_ref(), Some(expected));
}

fn limit_exceeded(failure: &FailedApply) -> TransactionResource {
    let receipt = rejected(failure);
    assert_eq!(
        receipt.result,
        InvokeHostFunctionResultCode::ResourceLimitExceeded
    );
    receipt
        .exceeded
        .expect("Kanatoko names the resource")
        .resource
}

fn invalid(failure: FailedApply) -> InvalidTransaction {
    match failure {
        FailedApply::Invalid(reason) => reason,
        other => panic!("expected a refused transaction, got {other:?}"),
    }
}

const AUTH_INVALID_ACTION: ScError = ScError::Auth(ScErrorCode::InvalidAction);
const AUTH_INVALID_INPUT: ScError = ScError::Auth(ScErrorCode::InvalidInput);

#[test]
fn network_config_and_signer_absence_are_captured() {
    let fixture = fixture();
    assert_eq!(fixture.provenance().protocol_version(), 30);
    let fork = fixture.fork();
    let config = fork.network_config().unwrap();
    let limits = config.limits();
    assert_eq!(limits.instructions, 400_000_000);
    assert!(limits.memory_bytes > 0);
    assert!(limits.footprint_entries > 0);
    assert!(limits.transaction_size_bytes > 0);
    assert!(limits.contract_code_size_bytes > 0);
    assert!(config.average_soroban_state_size_bytes() > 0);
    assert!(config.ledger_close_seconds() >= 1);
    assert_eq!(NetworkConfig::REQUIRED_SETTINGS.len(), 15);
    for label in SIGNERS {
        assert!(fork
            .ledger_entry(&signer(label).account_key())
            .unwrap()
            .is_none());
    }
    assert!(fork
        .ledger_entry(&existing_signer().account_key())
        .unwrap()
        .is_some());
}

#[test]
fn transactions_fail_closed_without_captured_network_config() {
    let mut fork = CapturedFixture::from_file(SOROSWAP_CAPTURE, MAINNET_PASSPHRASE)
        .unwrap()
        .fork();
    let source = signer("relayer");
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
        15
    );
    assert_eq!(
        missing(fork.prepare_transaction(&call, vec![]).unwrap_err()).len(),
        15
    );
    // This fixture never captured the signer's account.
    assert!(matches!(
        fork.create_local_account(&source, FUNDING),
        Err(TransactionError::Strict(
            StrictForkError::UnknownLedgerKeys { count: 1, .. }
        ))
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn signed_address_authorization_is_applied_in_the_next_ledger() {
    let relayer = signer("relayer");
    let user = signer("user");
    let (mut fork, candidate) = fork_with(&["relayer", "user"]);
    let call = TransactionCall::invoke(&relayer, &authorized_increment(&candidate, &user)).unwrap();
    let ledger = fork.ledger_sequence();
    let close_time = fork.ledger_close_time();

    let simulation = fork.simulate_transaction(&call).unwrap();
    assert_eq!(
        simulation.receipt().disposition,
        ReceiptDisposition::Previewed
    );
    assert_eq!(simulation.receipt().ledger_sequence, ledger);
    assert_eq!(simulation.authorization().len(), 1);
    assert_eq!(simulation.authorization()[0].address, user.address());
    assert!(matches!(
        simulation.authorization()[0].invocation.function,
        SorobanAuthorizedFunction::ContractFn(_)
    ));

    let before = fork.ledger_digest().unwrap();
    let authorization = fork
        .authorize(&simulation, &[&user], 7, ledger + 100)
        .unwrap();
    let prepared = fork.prepare_transaction(&call, authorization).unwrap();
    assert_eq!(prepared, prepare(&fork, &call, &[&user], 7));
    assert_eq!(fork.ledger_digest().unwrap(), before);
    let SorobanCredentials::Address(credentials) = &prepared.authorization()[0].credentials else {
        panic!("signed entries default to the SDKs' Address credentials");
    };
    assert_eq!(credentials.nonce, 7);
    let nonce_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: user.address(),
        key: ScVal::LedgerKeyNonce(ScNonceKey { nonce: 7 }),
        durability: ContractDataDurability::Temporary,
    });
    assert!(prepared.footprint_contains(&nonce_key));
    let data = prepared.transaction_data().clone();
    assert!(data.resources.instructions > 0);
    assert!(data.resource_fee > 0);

    let (balance, sequence) = account(&fork, &relayer);
    let receipt = fork.apply_transaction(&prepared).unwrap();
    assert_eq!(receipt.disposition, ReceiptDisposition::Committed);
    assert_eq!(receipt.result, InvokeHostFunctionResultCode::Success);
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));
    assert_eq!(receipt.events.len(), 1);
    assert_eq!(receipt.ledger_sequence, ledger + 1);
    assert_eq!(fork.ledger_sequence(), ledger + 1);
    let close_seconds = fork.network_config().unwrap().ledger_close_seconds();
    assert_eq!(fork.ledger_close_time(), close_time + close_seconds);
    assert_eq!(fork.ledger_digest().unwrap(), receipt.after_digest);
    assert!(receipt
        .state_changes
        .iter()
        .any(|change| change.key == nonce_key && change.after.is_some()));

    // The source paid the resource fee plus the base inclusion fee, minus
    // the refund, and its sequence number advanced.
    let resources = receipt.resources;
    assert!(resources.fee.non_refundable > 0);
    assert!(resources.fee.refundable >= resources.fee.rent);
    assert_eq!(receipt.fee_charged, resources.fee.total() + 100);
    assert!(receipt.fee_charged < data.resource_fee + 100);
    assert_eq!(
        account(&fork, &relayer),
        (balance - receipt.fee_charged, sequence + 1)
    );
    assert!(resources.instructions > 0);
    assert!(resources.instructions <= u64::from(data.resources.instructions));
    assert!(resources.write_bytes > 0);
    assert!(resources.write_bytes <= data.resources.write_bytes);
    assert_eq!(value(&mut fork, &candidate), ScVal::I64(46));

    // The consumed nonce exists now: the replay is included, charged, and
    // fails without any other change.
    let replay = fork.expect_failed_apply(&prepared).unwrap();
    rejected_with(&replay, &ScError::Auth(ScErrorCode::ExistingValue));
    assert_eq!(fork.ledger_sequence(), ledger + 2);
    assert_eq!(value(&mut fork, &candidate), ScVal::I64(46));

    // apply_transaction reports the same failure as an error after charging.
    let (balance, sequence) = account(&fork, &relayer);
    let Err(TransactionError::Rejected(receipt)) = fork.apply_transaction(&prepared) else {
        panic!("a failed transaction is an error");
    };
    assert_eq!(receipt.disposition, ReceiptDisposition::Rejected);
    assert_eq!(receipt.result, InvokeHostFunctionResultCode::Trapped);
    assert!(receipt.fee_charged > 0);
    assert_eq!(
        account(&fork, &relayer),
        (balance - receipt.fee_charged, sequence + 1)
    );
    assert_eq!(fork.ledger_sequence(), ledger + 3);
    assert_eq!(fork.ledger_digest().unwrap(), receipt.after_digest);
    assert_eq!(value(&mut fork, &candidate), ScVal::I64(46));
}

#[test]
fn address_v2_authorization_is_accepted_too() {
    let relayer = signer("relayer");
    let user = signer("user");
    let (mut fork, candidate) = fork_with(&["relayer", "user"]);
    let call = TransactionCall::invoke(&relayer, &authorized_increment(&candidate, &user)).unwrap();
    let simulation = fork.simulate_transaction(&call).unwrap();
    let expiration = fork.ledger_sequence() + 100;
    let authorization = fork
        .authorize_as(
            AuthorizationFormat::AddressV2,
            &simulation,
            &[&user],
            3,
            expiration,
        )
        .unwrap();
    assert!(matches!(
        authorization[0].credentials,
        SorobanCredentials::AddressV2(_)
    ));
    let prepared = fork.prepare_transaction(&call, authorization).unwrap();
    let receipt = fork.apply_transaction(&prepared).unwrap();
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));

    // The payloads differ: a V2 signature does not verify as Address.
    let tree = &simulation.authorization()[0];
    let v2 = fork
        .sign_authorization_as(AuthorizationFormat::AddressV2, tree, &user, 4, expiration)
        .unwrap();
    let SorobanCredentials::AddressV2(credentials) = v2.credentials else {
        panic!("AddressV2 requested");
    };
    let mislabeled = SorobanAuthorizationEntry {
        credentials: SorobanCredentials::Address(credentials),
        root_invocation: v2.root_invocation,
    };
    let prepared = prepare(&fork, &call, &[&user], 4).with_authorization(vec![mislabeled]);
    rejected_with(
        &fork.expect_failed_apply(&prepared).unwrap(),
        &AUTH_INVALID_ACTION,
    );
}

#[test]
fn source_account_authorization_needs_no_signature() {
    let user = signer("user");
    let (mut fork, candidate) = fork_with(&["user"]);
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
    let relayer = signer("relayer");
    fork.create_local_account(&relayer, FUNDING).unwrap();
    let from_relayer =
        TransactionCall::invoke(&relayer, &authorized_increment(&candidate, &user)).unwrap();
    let error = fork
        .prepare_transaction(&from_relayer, prepared.authorization().to_vec())
        .unwrap_err();
    assert!(matches!(error, TransactionError::SimulationFailed(_)));
}

#[test]
fn signing_failures_are_rejected_atomically() {
    let relayer = signer("relayer");
    let user = signer("user");
    let stranger = signer("stranger");
    let (mut fork, candidate) = fork_with(&["relayer", "user", "stranger"]);
    let call = TransactionCall::invoke(&relayer, &authorized_increment(&candidate, &user)).unwrap();
    let prepared = prepare(&fork, &call, &[&user], 11);
    let simulation = fork.simulate_transaction(&call).unwrap();
    let tree = simulation.authorization()[0].clone();
    let attempt = |fork: &mut StrictFork, signer: &Ed25519Signer, expiration: u32| {
        let entry = fork
            .sign_authorization(&tree, signer, 11, expiration)
            .unwrap();
        fork.expect_failed_apply(&prepared.clone().with_authorization(vec![entry]))
            .unwrap()
    };

    // A funded key that is not a signer of the user's account.
    let expiration = fork.ledger_sequence() + 100;
    rejected_with(
        &attempt(&mut fork, &stranger, expiration),
        &AUTH_INVALID_ACTION,
    );
    let wrong = fork
        .sign_authorization(&tree, &stranger, 11, expiration)
        .unwrap();
    assert!(matches!(
        fork.prepare_transaction(&call, vec![wrong]),
        Err(TransactionError::SimulationFailed(_))
    ));

    let failure = fork
        .expect_failed_apply(&prepared.clone().with_tampered_signature(0))
        .unwrap();
    rejected_with(&failure, &AUTH_INVALID_ACTION);

    // Expired, and expiring in the last closed ledger the simulation saw:
    // the transaction applies one ledger later.
    let past = fork.ledger_sequence() - 1;
    rejected_with(&attempt(&mut fork, &user, past), &AUTH_INVALID_INPUT);
    let current = fork.ledger_sequence();
    rejected_with(&attempt(&mut fork, &user, current), &AUTH_INVALID_INPUT);
    // Beyond the maximum entry TTL of the applying ledger.
    let far = fork.ledger_sequence() + 1 + MAX_ENTRY_TTL;
    rejected_with(&attempt(&mut fork, &user, far), &AUTH_INVALID_INPUT);

    let failure = fork
        .expect_failed_apply(&prepared.clone().without_authorization_entry(0))
        .unwrap();
    rejected_with(&failure, &AUTH_INVALID_ACTION);

    assert!(matches!(
        fork.authorize(&simulation, &[&stranger], 11, expiration),
        Err(TransactionError::MissingSigner { address }) if address == user.address()
    ));

    // None of the failures consumed the nonce.
    let receipt = fork.apply_transaction(&prepared).unwrap();
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::I64(46)));
}

#[test]
#[allow(clippy::too_many_lines)]
fn footprint_and_limits_are_enforced_like_core() {
    let relayer = signer("relayer");
    let user = signer("user");
    let (mut fork, candidate) = fork_with(&["relayer", "user"]);
    let call = TransactionCall::invoke(&relayer, &authorized_increment(&candidate, &user)).unwrap();
    let prepared = prepare(&fork, &call, &[&user], 3);
    let limits = fork.network_config().unwrap().limits();
    let value_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: candidate.clone(),
        key: ScVal::Symbol(ScSymbol("value".try_into().unwrap())),
        durability: ContractDataDurability::Persistent,
    });
    assert!(prepared.footprint_contains(&value_key));

    // The fee components of this exact transaction.
    let checkpoint = fork.checkpoint();
    let fee = fork.apply_transaction(&prepared).unwrap().resources.fee;
    fork.revert(checkpoint).unwrap();

    // A key outside the footprint fails inside the Host.
    let failure = fork
        .expect_failed_apply(&prepared.clone().without_footprint_key(&value_key))
        .unwrap();
    rejected_with(&failure, &ScError::Storage(ScErrorCode::ExceededLimit));

    // A written key declared read-only fails inside the Host.
    let read_only = edited(&prepared, |data| {
        let footprint = &mut data.resources.footprint;
        let mut read_write = footprint.read_write.to_vec();
        read_write.retain(|key| key != &value_key);
        let mut read_only = footprint.read_only.to_vec();
        read_only.push(value_key.clone());
        footprint.read_write = read_write.try_into().unwrap();
        footprint.read_only = read_only.try_into().unwrap();
    });
    rejected_with(
        &fork.expect_failed_apply(&read_only).unwrap(),
        &ScError::Storage(ScErrorCode::ExceededLimit),
    );

    // Declared instructions: the limit is accepted, one more is refused.
    let over = edited(&prepared, |data| {
        data.resources.instructions = u32::try_from(limits.instructions + 1).unwrap();
    });
    assert!(matches!(
        invalid(fork.expect_failed_apply(&over).unwrap()),
        InvalidTransaction::NetworkLimitExceeded {
            resource: TransactionResource::Instructions,
            ..
        }
    ));
    let at_limit = edited(&prepared, |data| {
        data.resources.instructions = u32::try_from(limits.instructions).unwrap();
        // 400M instructions cost 120,000 stroops at the captured rate.
        data.resource_fee += 130_000;
    });
    let checkpoint = fork.checkpoint();
    let committed = fork.apply_transaction(&at_limit).unwrap();
    assert_eq!(committed.disposition, ReceiptDisposition::Committed);
    fork.revert(checkpoint).unwrap();

    // Too few declared instructions exhaust the Host budget.
    let few = edited(&prepared, |data| data.resources.instructions = 10_000);
    assert_eq!(
        limit_exceeded(&fork.expect_failed_apply(&few).unwrap()),
        TransactionResource::Instructions
    );
    // Writing or reading more than declared fails the operation.
    let writes = edited(&prepared, |data| data.resources.write_bytes = 1);
    assert_eq!(
        limit_exceeded(&fork.expect_failed_apply(&writes).unwrap()),
        TransactionResource::WriteBytes
    );
    let reads = edited(&prepared, |data| data.resources.disk_read_bytes = 1);
    assert_eq!(
        limit_exceeded(&fork.expect_failed_apply(&reads).unwrap()),
        TransactionResource::DiskReadBytes
    );
    // A refundable fee that cannot pay for events and rent fails.
    let no_refundable = edited(&prepared, |data| data.resource_fee = fee.non_refundable);
    let failure = fork.expect_failed_apply(&no_refundable).unwrap();
    assert_eq!(
        rejected(&failure).result,
        InvokeHostFunctionResultCode::InsufficientRefundableFee
    );
    // A resource fee below the non-refundable fee is refused.
    let below = edited(&prepared, |data| data.resource_fee = fee.non_refundable - 1);
    assert!(matches!(
        invalid(fork.expect_failed_apply(&below).unwrap()),
        InvalidTransaction::InsufficientResourceFee { .. }
    ));
    // Duplicate footprint keys and invalid archived indices are refused.
    let duplicate = edited(&prepared, |data| {
        let mut read_only = data.resources.footprint.read_only.to_vec();
        read_only.push(value_key.clone());
        data.resources.footprint.read_only = read_only.try_into().unwrap();
    });
    assert!(matches!(
        invalid(fork.expect_failed_apply(&duplicate).unwrap()),
        InvalidTransaction::DuplicateFootprintKey(_)
    ));
    let archived = edited(&prepared, |data| {
        data.ext = SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
            archived_soroban_entries: vec![99].try_into().unwrap(),
        });
    });
    assert_eq!(
        invalid(fork.expect_failed_apply(&archived).unwrap()),
        InvalidTransaction::InvalidArchivedEntries
    );

    // An uncaptured footprint key is an environment error, not a failure.
    let uncaptured = edited(&prepared, |data| {
        let mut read_only = data.resources.footprint.read_only.to_vec();
        read_only.push(LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([0x77; 32]))),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
        }));
        data.resources.footprint.read_only = read_only.try_into().unwrap();
    });
    let before = fork.ledger_digest().unwrap();
    assert!(matches!(
        fork.expect_failed_apply(&uncaptured),
        Err(TransactionError::Strict(
            StrictForkError::UnknownLedgerKeys { count: 1, .. }
        ))
    ));
    assert_eq!(fork.ledger_digest().unwrap(), before);

    // A transaction that succeeds is reported, not committed.
    assert!(matches!(
        fork.expect_failed_apply(&prepared),
        Err(TransactionError::UnexpectedSuccess(receipt))
            if receipt.disposition == ReceiptDisposition::Previewed
    ));
    assert_eq!(fork.ledger_digest().unwrap(), before);
}

#[test]
#[allow(clippy::too_many_lines)]
fn envelope_and_source_validity_follow_core() {
    let relayer = signer("relayer");
    let user = signer("user");
    let (mut fork, candidate) = fork_with(&["relayer", "user"]);
    let increment = authorized_increment(&candidate, &user);
    let call = TransactionCall::invoke(&relayer, &increment).unwrap();
    let prepared = prepare(&fork, &call, &[&user], 5);
    let envelope = |call: TransactionCall| prepared.clone().with_call(call);

    let memo = envelope(call.clone().with_memo(Memo::Id(7)));
    assert_eq!(
        invalid(fork.expect_failed_apply(&memo).unwrap()),
        InvalidTransaction::MemoOrMuxedSource
    );
    let muxed = envelope(call.clone().with_muxed_source(7));
    assert_eq!(
        invalid(fork.expect_failed_apply(&muxed).unwrap()),
        InvalidTransaction::MemoOrMuxedSource
    );
    let cheap = envelope(call.clone().with_inclusion_fee(99));
    assert!(matches!(
        invalid(fork.expect_failed_apply(&cheap).unwrap()),
        InvalidTransaction::InsufficientInclusionFee {
            bid: 99,
            minimum: 100
        }
    ));
    let late = envelope(call.clone().with_time_bounds(Some(TimeBounds {
        min_time: TimePoint(0),
        max_time: TimePoint(fork.ledger_close_time()),
    })));
    assert!(matches!(
        invalid(fork.expect_failed_apply(&late).unwrap()),
        InvalidTransaction::TooLate { .. }
    ));
    let early = envelope(call.clone().with_time_bounds(Some(TimeBounds {
        min_time: TimePoint(fork.ledger_close_time() + 3_600),
        max_time: TimePoint(0),
    })));
    assert!(matches!(
        invalid(fork.expect_failed_apply(&early).unwrap()),
        InvalidTransaction::TooEarly { .. }
    ));

    // A source captured absent and never created.
    let spare_call = TransactionCall::invoke(&signer("spare"), &increment).unwrap();
    let missing = fork
        .prepare_transaction(&spare_call, prepared.authorization().to_vec())
        .unwrap();
    assert_eq!(
        invalid(fork.expect_failed_apply(&missing).unwrap()),
        InvalidTransaction::SourceAccountMissing
    );

    // A source that cannot pay the full fee above its reserve.
    let poor = signer("poor");
    fork.create_local_account(&poor, MINIMUM_BALANCE + 1_000)
        .unwrap();
    let poor_call = TransactionCall::invoke(&poor, &increment).unwrap();
    let poor_prepared = fork
        .prepare_transaction(&poor_call, prepared.authorization().to_vec())
        .unwrap();
    assert!(matches!(
        invalid(fork.expect_failed_apply(&poor_prepared).unwrap()),
        InvalidTransaction::InsufficientBalance {
            available: 1_000,
            ..
        }
    ));

    // More signatures make a larger envelope and a larger fee.
    let checkpoint = fork.checkpoint();
    let base = fork.apply_transaction(&prepared).unwrap();
    fork.revert(checkpoint).unwrap();
    let many = fork
        .apply_transaction(&envelope(call.clone().with_signature_count(20)))
        .unwrap();
    assert_eq!(
        many.resources.transaction_size_bytes,
        base.resources.transaction_size_bytes + 19 * 72
    );
    assert!(many.resources.fee.non_refundable > base.resources.fee.non_refundable);
}

#[test]
fn upload_and_create_contract_v2_run_the_constructor_under_real_auth() {
    let deployer = signer("deployer");
    let mut fork = fixture().fork();
    fork.create_local_account(&deployer, FUNDING).unwrap();
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

    let salt = DEPLOYMENTS[0].1;
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
    assert_eq!(value(&mut fork, &expected), ScVal::I64(41));

    // The created contract serves later transactions.
    let user = signer("user");
    fork.create_local_account(&user, FUNDING).unwrap();
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
    let relayer = signer("relayer");
    let deployer = signer("deployer");
    let mut fork = fixture().fork();
    for account in [&relayer, &deployer] {
        fork.create_local_account(account, FUNDING).unwrap();
    }
    let wasm_hash: [u8; 32] = Sha256::digest(STATEFUL_WASM).into();
    let upload = TransactionCall::upload_wasm(&relayer, STATEFUL_WASM).unwrap();
    fork.apply_transaction(&prepare(&fork, &upload, &[], 0))
        .unwrap();

    let salt = DEPLOYMENTS[1].1;
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
        SorobanCredentials::Address(_)
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
fn uncaptured_code_and_deployments_fail_closed() {
    let deployer = signer("deployer");
    let mut fork = fixture().fork();
    fork.create_local_account(&deployer, FUNDING).unwrap();
    let upload = TransactionCall::upload_wasm(&deployer, QUOTE_FORWARDER_WASM).unwrap();
    assert!(matches!(
        fork.simulate_transaction(&upload),
        Err(TransactionError::Strict(
            StrictForkError::UnknownLedgerKeys { .. }
        ))
    ));

    let wasm_hash: [u8; 32] = Sha256::digest(STATEFUL_WASM).into();
    let upload = TransactionCall::upload_wasm(&deployer, STATEFUL_WASM).unwrap();
    fork.apply_transaction(&prepare(&fork, &upload, &[], 0))
        .unwrap();
    let create = TransactionCall::create_contract(
        &deployer,
        deployer.address(),
        [0x99; 32],
        wasm_hash,
        vec![ScVal::I64(1)],
    )
    .unwrap();
    assert!(matches!(
        fork.simulate_transaction(&create),
        Err(TransactionError::Strict(
            StrictForkError::UnknownLedgerKeys { .. }
        ))
    ));
}

#[test]
fn signed_native_xlm_transfer_charges_the_source() {
    let sender = signer("sender");
    let receiver = signer("receiver");
    let mut fork = fixture().fork();
    for account in [&sender, &receiver] {
        fork.create_local_account(account, FUNDING).unwrap();
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
    // The receiver submits; the sender signs its own transfer.
    let call = TransactionCall::invoke(&receiver, &transfer).unwrap();
    let receipt = fork
        .apply_transaction(&prepare(&fork, &call, &[&sender], 1))
        .unwrap();
    assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::Void));
    assert!(receipt.resources.disk_read_entries >= 2);
    assert!(receipt.resources.disk_read_bytes > 0);
    assert_eq!(account(&fork, &sender).0, FUNDING - 25);
    assert_eq!(
        account(&fork, &receiver).0,
        FUNDING + 25 - receipt.fee_charged
    );
}

#[test]
fn successive_applies_close_successive_ledgers() {
    let user = signer("user");
    let (mut fork, candidate) = fork_with(&["user"]);
    let ledger = fork.ledger_sequence();
    let call = TransactionCall::invoke(&user, &authorized_increment(&candidate, &user)).unwrap();
    for step in 1..=3 {
        let receipt = fork
            .apply_transaction(&prepare(&fork, &call, &[], 0))
            .unwrap();
        assert_eq!(receipt.ledger_sequence, ledger + step);
        assert_eq!(fork.ledger_sequence(), ledger + step);
    }
    assert_eq!(value(&mut fork, &candidate), ScVal::I64(56));
}

#[test]
fn invoke_with_source_accepts_source_account_credentials() {
    let user = signer("user");
    let (mut fork, candidate) = fork_with(&["user"]);
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
    let user = signer("user");
    let (mut fork, candidate) = fork_with(&["user"]);
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

    let user = signer("user");
    fork.create_local_account(&user, FUNDING).unwrap();
    assert!(matches!(
        fork.create_local_account(&user, FUNDING),
        Err(TransactionError::LocalAccountExists)
    ));
    // A real Mainnet account is never shadowed.
    assert!(matches!(
        fork.create_local_account(&existing_signer(), FUNDING),
        Err(TransactionError::LocalAccountExists)
    ));
    // An account the capture never looked at is unknown.
    assert!(matches!(
        fork.create_local_account(&signer("never captured"), FUNDING),
        Err(TransactionError::Strict(
            StrictForkError::UnknownLedgerKeys { .. }
        ))
    ));
    assert!(matches!(
        fork.create_local_account(&signer("spare"), 1),
        Err(TransactionError::LocalAccountBalanceTooLow { .. })
    ));
}

/// The README sample, replayed from the committed capture.
#[test]
fn readme_signed_transfer() -> Result<(), Box<dyn std::error::Error>> {
    let sender = Ed25519Signer::from_label("sender");
    let relayer = Ed25519Signer::from_label("relayer");
    let run = runner().offline().run(scenario)?;
    let mut fork = run.fixture().fork();
    fork.create_local_account(&sender, 100_000_000)?;
    fork.create_local_account(&relayer, 100_000_000)?;

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
    assert_eq!(receipt.disposition, ReceiptDisposition::Committed);
    assert!(receipt.fee_charged > 0);

    let replay = fork.expect_failed_apply(&transaction)?;
    let FailedApply::Rejected(failed) = replay else {
        panic!("the replay is included and fails");
    };
    assert_eq!(failed.result, InvokeHostFunctionResultCode::Trapped);
    Ok(())
}
