#![cfg(feature = "capture")]

use kanatoko::{mainnet, CacheStatus, ScenarioFork};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    AlphaNum4, AssetCode4, ContractEventBody, ContractExecutable, LedgerEntryData, ScAddress,
    ScVal, TrustLineAsset, TrustLineFlags,
};
use soroban_sdk::{
    testutils::{EnvTestConfig, Events as _, MuxedAddress as _},
    Address, Env, MuxedAddress,
};

mod pool_abi {
    #![allow(clippy::too_many_arguments)]

    // Deliberately import a different executable that exposes the same pool
    // methods. It generates Rust bindings only: calls target the captured
    // network pool address and execute the captured network pool WASM.
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_aquarius_wrapper.wasm",
        sha256 = "798c959e1e22093c49b4ec6636aafed14e889614fb243426abe5023b30c17520",
    );
}

mod incompatible_abi {
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
        sha256 = "6f6f469798b686cc485ad207f32e3f77009c4b69ab2437d9bdca97f149b54ba8",
    );
}

const CAPTURE: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/auto-capture.json";
const POOL: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const USDC_ISSUER: &str = "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN";
const REAL_ACCOUNT: &str = "GBJGQAC4PP3MFD3FOAODOZAWIRKZGZCHAU35EUPKJGJOLMNZM7HU4U5D";
const ONE_USDC: u128 = 10_000_000;
const MUXED_ID: u64 = 42;
const ABI_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_aquarius_wrapper.wasm");

#[test]
fn one_scenario_mixes_abi_client_and_dynamic_invoke_without_manual_capture() {
    let run = mainnet()
        .cache(CAPTURE)
        .offline()
        .run(full_scenario)
        .unwrap();

    assert_eq!(run.cache_status(), CacheStatus::Hit);
    assert_eq!(run.fixture().report().final_replay_rpc_reads(), 0);
    assert_ne!(
        captured_contract_wasm_hash(run.fixture(), POOL),
        <[u8; 32]>::from(Sha256::digest(ABI_WASM)),
        "the ABI source WASM must not replace the captured network executable",
    );
    assert_captured_real_account_state(run.fixture());
}

#[test]
fn incompatible_local_abi_fails_instead_of_replacing_network_wasm() {
    mainnet()
        .cache(CAPTURE)
        .offline()
        .run(|fork| {
            let pool_id = fork.contract(POOL);
            let incompatible = incompatible_abi::Client::new(fork.env(), &pool_id);
            assert!(incompatible.try_get().is_err());
        })
        .unwrap();
}

fn full_scenario(fork: &ScenarioFork<'_>) {
    price_moves(fork);
    real_account_and_muxed_destination(fork);
}

fn price_moves(fork: &ScenarioFork<'_>) {
    let env = fork.env();
    let user = fork.local_account("swap-user");
    fork.fund_local_account(&user, 100_000_000);
    let pool_id = fork.contract(POOL);
    let usdc = fork.contract(USDC);
    let pool = pool_abi::Client::new(env, &pool_id);
    assert!(matches!(ScAddress::from(&user), ScAddress::Account(_)));

    let before = pool.estimate_swap(&1, &0, &ONE_USDC);
    let reserves = pool.get_reserves();
    let amount = reserves.get(1).unwrap() / 10;
    let amount_i128 = i128::try_from(amount).unwrap();

    fork.mock_all_auths();
    let admin = fork.invoke::<Address>(&usdc, "admin", ());
    fork.invoke::<()>(&usdc, "trust", (user.clone(),));
    fork.invoke::<()>(&usdc, "set_authorized", (user.clone(), true));
    fork.invoke::<()>(&usdc, "mint", (user.clone(), amount_i128));
    assert_eq!(env.auths()[0].0, admin);

    let received = fork.invoke::<u128>(&pool_id, "swap", (user, 1_u32, 0_u32, amount, 0_u128));
    assert!(received > 0);

    let after = pool.estimate_swap(&1, &0, &ONE_USDC);
    assert_ne!(after, before);
    assert!(after < before);
}

fn real_account_and_muxed_destination(fork: &ScenarioFork<'_>) {
    let env = fork.env();
    let pool_id = fork.contract(POOL);
    let pool = pool_abi::Client::new(env, &pool_id);
    let tokens = pool.get_tokens();
    let xlm = tokens.get(0).unwrap();
    let usdc = tokens.get(1).unwrap();
    let real_account = fork.account(REAL_ACCOUNT);

    let xlm_before = fork.invoke::<i128>(&xlm, "balance", (real_account.clone(),));
    let usdc_balance = fork.invoke::<i128>(&usdc, "balance", (real_account.clone(),));
    assert!(xlm_before > 0);
    assert!(usdc_balance >= 0);

    let sender = fork.local_account("muxed-sender");
    fork.fund_local_account(&sender, 100_000_000);
    assert_eq!(
        fork.invoke::<i128>(&xlm, "balance", (sender.clone(),)),
        100_000_000
    );
    let muxed = MuxedAddress::new(real_account.clone(), MUXED_ID);
    let muxed = fork.muxed_account(&muxed.to_strkey().to_string());
    assert_eq!(muxed.address(), real_account);
    assert_eq!(muxed.id(), Some(MUXED_ID));

    fork.mock_all_auths();
    fork.invoke::<()>(&xlm, "transfer", (sender, muxed, 1_i128));
    assert_muxed_transfer_event(env);

    let xlm_after = fork.invoke::<i128>(&xlm, "balance", (real_account,));
    assert_eq!(xlm_after, xlm_before + 1);
}

fn assert_muxed_transfer_event(env: &Env) {
    let events = env.events().all();
    let event = events
        .events()
        .last()
        .expect("muxed transfer must emit an event");
    let ContractEventBody::V0(body) = &event.body;
    let ScVal::Map(Some(data)) = &body.data else {
        panic!("muxed transfer event data must be a map");
    };
    assert!(data.iter().any(|entry| {
        matches!(
            (&entry.key, &entry.val),
            (ScVal::Symbol(symbol), ScVal::U64(id))
                if symbol.0.as_slice() == b"to_muxed_id" && *id == MUXED_ID
        )
    }));
}

fn assert_captured_real_account_state(fixture: &kanatoko::CapturedFixture) {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    let ScAddress::Account(account_id) = ScAddress::from(&Address::from_str(&env, REAL_ACCOUNT))
    else {
        unreachable!("REAL_ACCOUNT must be a G-address");
    };
    let ScAddress::Account(issuer_id) = ScAddress::from(&Address::from_str(&env, USDC_ISSUER))
    else {
        unreachable!("USDC_ISSUER must be a G-address");
    };
    let usdc_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"USDC"),
        issuer: issuer_id,
    });
    let mut account_state = None;
    let mut usdc_state = None;
    for (_, (entry, _)) in &fixture.frozen_fixture().ledger_snapshot().ledger_entries {
        match &entry.data {
            LedgerEntryData::Account(account) if account.account_id == account_id => {
                account_state = Some((account.balance, account.seq_num.0, account.num_sub_entries));
            }
            LedgerEntryData::Trustline(trustline)
                if trustline.account_id == account_id && trustline.asset == usdc_asset =>
            {
                usdc_state = Some((trustline.balance, trustline.limit, trustline.flags));
            }
            _ => {}
        }
    }
    let (xlm_balance, sequence, subentries) =
        account_state.expect("real AccountEntry must be captured");
    assert!(xlm_balance > 0);
    assert!(sequence > 0);
    assert!(subentries > 0);

    let (usdc_balance, limit, flags) =
        usdc_state.expect("real USDC TrustLineEntry must be captured");
    assert!(usdc_balance >= 0);
    assert!(limit > 0);
    assert_ne!(flags & (TrustLineFlags::AuthorizedFlag as u32), 0);
}

fn captured_contract_wasm_hash(fixture: &kanatoko::CapturedFixture, contract: &str) -> [u8; 32] {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    let contract: ScAddress = (&Address::from_str(&env, contract)).into();
    fixture
        .frozen_fixture()
        .ledger_snapshot()
        .ledger_entries
        .iter()
        .find_map(|(_, (entry, _))| {
            let LedgerEntryData::ContractData(data) = &entry.data else {
                return None;
            };
            if data.contract != contract {
                return None;
            }
            let ScVal::ContractInstance(instance) = &data.val else {
                return None;
            };
            let ContractExecutable::Wasm(hash) = &instance.executable else {
                return None;
            };
            Some(hash.0)
        })
        .expect("captured contract must reference network WASM")
}
