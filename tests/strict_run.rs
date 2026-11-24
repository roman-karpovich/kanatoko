#![cfg(all(feature = "capture", kanatoko_protocol_30_fixtures))]

//! The typed transaction layer on a discovering strict run.
//!
//! `refresh_strict_run_fixture` captures the whole flow below once with
//! `run_strict`; every other test replays a part of it offline against that
//! one shared fixture. A part touches only keys the full flow touched, so
//! strict replay needs no network.

use kanatoko::{
    mainnet, symbol, vec_val, AutoRunner, CacheStatus, Ed25519Signer, FailedApply, InvokeErrorKind,
    ReceiptDisposition, StrictFork, TransactionCall, TransactionError,
};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    ContractDataDurability, ContractExecutable, Hash, HostFunction, InvokeHostFunctionResultCode,
    MuxedContract, ScAddress, ScError, ScErrorCode, ScSymbol, ScVal,
};

const CAPTURE: &str = "fixtures/mainnet/strict-run-p30/auto-capture.json";
const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const APP_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_stateful_fixture.wasm");
const FUNDING: i64 = 1_000 * 10_000_000;

type Outcome<T> = Result<T, TransactionError>;

fn runner() -> AutoRunner {
    mainnet().cache(CAPTURE)
}

fn signer(label: &str) -> Ed25519Signer {
    Ed25519Signer::from_label(label)
}

fn xlm() -> ScAddress {
    XLM.parse().unwrap()
}

/// Accounts and the deployed app every part starts from.
fn setup(fork: &mut StrictFork) -> Outcome<ScAddress> {
    let (owner, other) = (signer("strict owner"), signer("strict other"));
    fork.create_local_accounts(&[&owner, &other], FUNDING)?;
    Ok(fork.deploy(&owner, APP_WASM, [1; 32], (41_i64,))?.address)
}

fn increment(owner: &Ed25519Signer, app: &ScAddress) -> Outcome<TransactionCall> {
    TransactionCall::invoke_fn(owner, app, "authorized_increment", (owner, 5_i64))
}

fn signed_call(fork: &mut StrictFork) -> Outcome<i64> {
    let owner = signer("strict owner");
    let app = setup(fork)?;
    let receipt = fork.apply_signed(&increment(&owner, &app)?, &[&owner])?;
    assert_eq!(receipt.disposition, ReceiptDisposition::Committed);
    let events = receipt.events_from(&app, "increment");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].topics()[0], symbol("increment"));
    assert!(receipt
        .to_string()
        .starts_with("Committed Success in ledger"));
    fork.view::<i64>(&app, "get", ())
}

fn negatives(fork: &mut StrictFork) -> Outcome<()> {
    let (owner, other) = (signer("strict owner"), signer("strict other"));
    let app = setup(fork)?;
    // The other account submits; the owner signs its own authorization.
    let call = TransactionCall::invoke_fn(&other, &app, "authorized_increment", (&owner, 5_i64))?;
    let prepared = fork.sign(&call, &[&owner])?;

    // Signed by a key that is not the owner's signer.
    let wrong = prepared.clone().resigned_by(fork, 0, &other)?;
    let failed = fork.expect_failed_apply(&wrong)?;
    assert_eq!(
        failed.host_error(),
        Some(&ScError::Auth(ScErrorCode::InvalidAction))
    );

    // A call whose own simulation fails, built on valid declarations: Core
    // includes it and the contract's error fails it.
    let HostFunction::InvokeContract(mut invoke) = prepared.call().function().clone() else {
        unreachable!("a contract call");
    };
    invoke.function_name = ScSymbol("increment_then_fail".try_into().unwrap());
    invoke.args = vec![5_i64.into()].try_into().unwrap();
    let failing = prepared
        .clone()
        .with_function(HostFunction::InvokeContract(invoke))
        .with_authorization(vec![]);
    let failed = fork.expect_failed_apply(&failing)?;
    let FailedApply::Rejected(receipt) = &failed else {
        panic!("an included, failed call");
    };
    assert_eq!(receipt.result, InvokeHostFunctionResultCode::Trapped);
    assert_eq!(failed.host_error(), Some(&ScError::Contract(1)));

    // Other arguments invalidate the owner's signature.
    let rewritten = prepared.clone().with_args((&owner, 6_i64));
    let failed = fork.expect_failed_apply(&rewritten)?;
    assert_eq!(
        failed.host_error(),
        Some(&ScError::Auth(ScErrorCode::InvalidAction))
    );

    // The untouched transaction still applies.
    fork.apply_transaction(&prepared)?;
    Ok(())
}

/// Stored value, the app's and the owner's XLM, the owner's native balance,
/// and the app's WASM hash.
type State = (i64, i128, i128, Option<i64>, [u8; 32]);

fn state(fork: &mut StrictFork) -> Outcome<State> {
    let owner = signer("strict owner");
    let app = setup(fork)?;
    let durability = ContractDataDurability::Persistent;
    fork.update_contract_data(&app, symbol("value"), durability, |_, value: &mut i64| {
        *value += 100;
    })?;
    let stored = fork
        .contract_data::<i64>(&app, symbol("value"), durability)?
        .unwrap_or_default();
    // An override inside a scope is reverted after it.
    let scoped = fork.scoped(|fork| -> Outcome<i64> {
        fork.set_contract_data(&app, symbol("value"), durability, Some(0_i64.into()))?;
        fork.view(&app, "get", ())
    })?;
    assert_eq!(scoped, 0);
    assert_eq!(fork.view::<i64>(&app, "get", ())?, stored);

    fork.set_token_balance(&xlm(), &app, 25)?;
    fork.set_token_balance(&xlm(), &owner.address(), 2 * i128::from(FUNDING))?;
    let ContractExecutable::Wasm(Hash(hash)) = fork.contract_instance(&app)?.unwrap().executable
    else {
        panic!("a WASM contract");
    };
    Ok((
        stored,
        fork.token_balance(&xlm(), &app)?,
        fork.token_balance(&xlm(), &owner.address())?,
        fork.account_balance(&owner.account_id())?,
        hash,
    ))
}

/// The README sample.
fn readme(fork: &mut StrictFork) -> Result<(), Box<dyn std::error::Error>> {
    let owner = Ed25519Signer::from_label("strict owner");
    let other = Ed25519Signer::from_label("strict other");
    fork.create_local_accounts(&[&owner, &other], 1_000 * 10_000_000)?;

    let app = fork.deploy(&owner, APP_WASM, [1; 32], (41_i64,))?.address;
    let call = TransactionCall::invoke_fn(&owner, &app, "authorized_increment", (&owner, 5_i64))?;
    let receipt = fork.apply_signed(&call, &[&owner])?;
    println!("{receipt}");
    assert_eq!(fork.view::<i64>(&app, "get", ())?, 46);

    let xlm: ScAddress = XLM.parse()?;
    fork.set_token_balance(&xlm, &app, 25)?;
    assert_eq!(fork.token_balance(&xlm, &app)?, 25);
    Ok(())
}

/// Protocol 30 carries a muxed contract address through signed SAC transfer.
fn transfer_to_muxed_contract(fork: &mut StrictFork) -> Outcome<()> {
    let owner = signer("strict owner");
    let app = setup(fork)?;
    let ScAddress::Contract(contract_id) = app.clone() else {
        unreachable!("setup deployed a contract")
    };
    let recipient = ScAddress::MuxedContract(MuxedContract {
        id: 17,
        contract_id,
    });
    let before = fork.token_balance(&xlm(), &app)?;
    let call =
        TransactionCall::invoke_fn(&owner, &xlm(), "transfer", (&owner, recipient, 25_i128))?;
    let receipt = fork.apply_signed(&call, &[&owner])?;
    assert_eq!(receipt.disposition, ReceiptDisposition::Committed);
    assert_eq!(fork.token_balance(&xlm(), &app)?, before + 25);
    Ok(())
}

#[test]
fn signed_sac_transfer_credits_the_base_contract_of_a_muxed_recipient() {
    runner()
        .offline()
        .run_strict(transfer_to_muxed_contract)
        .unwrap()
        .into_value()
        .unwrap();
}

/// The whole flow the shared fixture holds.
fn full(fork: &mut StrictFork) -> Outcome<()> {
    fork.scoped(signed_call)?;
    fork.scoped(transfer_to_muxed_contract)?;
    fork.scoped(negatives)?;
    fork.scoped(state)?;
    fork.scoped(readme).expect("the README sample passes");
    Ok(())
}

#[test]
#[ignore = "read-only Mainnet capture; run explicitly to refresh the fixture"]
fn refresh_strict_run_fixture() {
    let run = runner().refresh().run_strict(full).unwrap();
    assert!(matches!(
        run.cache_status(),
        CacheStatus::Created | CacheStatus::Refreshed
    ));
    run.into_value().unwrap();
}

#[test]
fn readme_sample_replays_offline() {
    runner()
        .offline()
        .run_strict(readme)
        .unwrap()
        .into_value()
        .unwrap();
}

#[test]
fn signed_deploy_call_and_events() {
    let run = runner().offline().run_strict(signed_call).unwrap();
    assert_eq!(run.cache_status(), CacheStatus::Hit);
    assert_eq!(run.into_value().unwrap(), 46);
}

#[test]
fn negative_variants_fail_like_core() {
    runner()
        .offline()
        .run_strict(negatives)
        .unwrap()
        .into_value()
        .unwrap();
}

#[test]
fn typed_state_reads_overrides_and_token_balances() {
    let (stored, app_xlm, owner_xlm, owner_balance, hash) = runner()
        .offline()
        .run_strict(state)
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(stored, 141);
    assert_eq!(app_xlm, 25);
    assert_eq!(owner_xlm, 2 * i128::from(FUNDING));
    assert_eq!(owner_balance, Some(2 * FUNDING));
    assert_eq!(hash, <[u8; 32]>::from(Sha256::digest(APP_WASM)));
}

#[test]
fn the_full_flow_replays_identically() {
    let first = runner().offline().run_strict(signed_call).unwrap();
    let second = runner().offline().run_strict(signed_call).unwrap();
    assert_eq!(first.into_value().unwrap(), second.into_value().unwrap());
    runner()
        .offline()
        .run_strict(full)
        .unwrap()
        .into_value()
        .unwrap();
}

#[test]
fn views_report_typed_failures() {
    let error = runner()
        .offline()
        .run_strict(|fork| -> Outcome<()> {
            let app = setup(fork)?;
            // `get` returns i64, not an address.
            match fork.view::<kanatoko::soroban_sdk::Address>(&app, "get", ()) {
                Err(TransactionError::Invocation(failure)) => {
                    assert_eq!(failure.kind, InvokeErrorKind::ResultConversion);
                }
                other => panic!("expected a conversion failure, got {other:?}"),
            }
            fork.view::<i64>(&app, "missing", ()).map(|_| ())
        })
        .unwrap()
        .into_value()
        .unwrap_err();
    assert!(matches!(error, TransactionError::Invocation(_)));
    // Keys of a vector value are built without an Env.
    assert_eq!(
        vec_val((symbol("Balance"), 1_u32)),
        soroban_env_host::xdr::ScVal::Vec(Some(
            vec![symbol("Balance"), 1_u32.into()].try_into().unwrap()
        ))
    );
}

mod typed {
    use kanatoko::soroban_sdk::{self, contracttype, Address, Map, Symbol, Val};

    #[contracttype]
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct Position {
        pub owner: Address,
        pub amount: i128,
    }

    #[contracttype]
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum DataKey {
        Position(Address),
    }

    pub type Fields = Map<Symbol, Val>;
}

#[test]
fn contract_types_convert_without_an_env() {
    use kanatoko::soroban_sdk::{Address, Symbol, TryFromVal};
    use kanatoko::{from_sc_val, sdk, to_sc_val};
    use typed::{DataKey, Fields, Position};

    let run = runner()
        .offline()
        .run_strict(|fork| -> Result<(), Box<dyn std::error::Error>> {
            let owner = signer("strict owner");
            let app = setup(fork)?;
            let position = |env: &kanatoko::soroban_sdk::Env| Position {
                owner: Address::from_str(env, XLM),
                amount: 7,
            };
            let encoded = to_sc_val(position)?;
            let decoded = from_sc_val::<Position>(&encoded)?;
            assert_eq!(decoded.amount, 7);
            assert_eq!(decoded.owner, Address::from_str(decoded.owner.env(), XLM));

            // An enum key and a struct value, written, read, and updated.
            let durability = ContractDataDurability::Persistent;
            let key = to_sc_val(|env| DataKey::Position(Address::from_str(env, XLM)))?;
            fork.set_contract_data(&app, key.clone(), durability, Some(encoded))?;
            fork.update_contract_data(
                &app,
                key.clone(),
                durability,
                |_, stored: &mut Position| {
                    stored.amount += 1;
                },
            )?;
            let stored = fork.contract_data::<Position>(&app, key.clone(), durability)?;
            assert_eq!(stored.map(|stored| stored.amount), Some(8));
            let same_key = sdk(|env| DataKey::Position(Address::from_str(env, XLM)));
            assert!(fork
                .contract_data::<Position>(&app, same_key, durability)?
                .is_some());

            // Typed events, exactly one, and measured headroom.
            let receipt = fork.apply_signed(&increment(&owner, &app)?, &[&owner])?;
            let event = receipt.single_event(&app, "increment")?;
            assert_eq!(
                event
                    .topic_as::<kanatoko::soroban_sdk::Symbol>(0)?
                    .to_string(),
                "increment"
            );
            let fields = event.data_as::<Fields>()?;
            let value = fields.get(Symbol::new(fields.env(), "value")).unwrap();
            let value = i64::try_from_val(fields.env(), &value).unwrap();
            assert_eq!(value, 46);
            assert!(matches!(
                receipt.single_event(&app, "reject"),
                Err(TransactionError::EventCount { found: 0 })
            ));
            let limits = fork.network_config()?.limits();
            receipt.resources.check_within(&limits.scaled(50))?;
            assert!(receipt.resources.check_within(&limits.scaled(0)).is_err());
            Ok(())
        })
        .unwrap();
    run.into_value().unwrap();
}

#[test]
fn values_holding_objects_of_another_env_are_refused() {
    use kanatoko::soroban_sdk::{testutils::EnvTestConfig, vec, Address, Env, Map, Symbol};
    use kanatoko::{from_sc_val, sdk, to_sc_val, ConversionError, IntoScVal};
    use typed::Position;

    let other = Env::new_with_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    // Handle by handle, this address shadows the first object a conversion
    // makes, so converting it by its handle would name that object instead.
    let foreign = Address::from_str(&other, XLM);
    let owner_address = signer("strict owner").address();
    let owner = owner_address.to_string();

    assert_eq!(to_sc_val(|_| foreign.clone()), Err(ConversionError));
    assert_eq!(
        to_sc_val(|env| vec![env, Address::from_str(env, &owner), foreign.clone()]),
        Err(ConversionError)
    );
    assert_eq!(
        to_sc_val(|env| Map::from_array(env, [(Symbol::new(env, "owner"), foreign.clone())])),
        Err(ConversionError)
    );
    assert_eq!(
        to_sc_val(|_| Position {
            owner: foreign.clone(),
            amount: 1,
        }),
        Err(ConversionError)
    );
    let whole = vec![&other, foreign.clone()];
    assert_eq!(to_sc_val(|_| whole.clone()), Err(ConversionError));
    let decoded: Address =
        from_sc_val(&to_sc_val(|env| Address::from_str(env, XLM)).unwrap()).unwrap();
    assert_eq!(to_sc_val(|_| decoded.clone()), Err(ConversionError));
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sdk(|_| foreign.clone()).into_sc_val()
    }));
    assert!(refused.is_err());

    // The same values built in the given environment convert.
    let built = to_sc_val(|env| {
        vec![
            env,
            Address::from_str(env, &owner),
            Address::from_str(env, XLM),
        ]
    })
    .unwrap();
    let expected = ScVal::Vec(Some(
        std::vec![ScVal::Address(owner_address), ScVal::Address(xlm()),]
            .try_into()
            .unwrap(),
    ));
    assert_eq!(built, expected);
}

#[test]
fn rejections_are_asserted_in_one_call() {
    runner()
        .offline()
        .run_strict(|fork| -> Outcome<()> {
            let (owner, other) = (signer("strict owner"), signer("strict other"));
            let app = setup(fork)?;
            let call =
                TransactionCall::invoke_fn(&other, &app, "authorized_increment", (&owner, 5_i64))?;
            let wrong = fork.sign(&call, &[&owner])?.resigned_by(fork, 0, &other)?;
            let receipt =
                fork.expect_rejected_with(&wrong, &ScError::Auth(ScErrorCode::InvalidAction))?;
            assert_eq!(receipt.disposition, ReceiptDisposition::Rejected);

            // Another error leaves the fork as it was, fee and sequence
            // number included, so the same transaction still applies.
            let again = fork.sign(&call, &[&owner])?.resigned_by(fork, 0, &other)?;
            let before = fork.ledger_digest().unwrap();
            assert!(matches!(
                fork.expect_rejected_with(&again, &ScError::Contract(1)),
                Err(TransactionError::UnexpectedFailure(failure))
                    if matches!(*failure, FailedApply::Rejected(_))
            ));
            assert_eq!(fork.ledger_digest().unwrap(), before);
            fork.expect_rejected_with(&again, &ScError::Auth(ScErrorCode::InvalidAction))?;
            assert_ne!(fork.ledger_digest().unwrap(), before);
            Ok(())
        })
        .unwrap()
        .into_value()
        .unwrap();
}
