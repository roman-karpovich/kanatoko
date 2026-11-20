#![cfg(all(feature = "capture", kanatoko_protocol_29_fixtures))]

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
    ScAddress, ScError, ScErrorCode, ScSymbol,
};

const CAPTURE: &str = "fixtures/mainnet/strict-run-p29/auto-capture.json";
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
    fork.update_contract_data(&app, symbol("value"), durability, |value: &mut i64| {
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

/// The whole flow the shared fixture holds.
fn full(fork: &mut StrictFork) -> Outcome<()> {
    fork.scoped(signed_call)?;
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
