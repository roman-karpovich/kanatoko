#![cfg(feature = "capture")]

use kanatoko::{mainnet, CacheStatus, ScenarioFork};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{ContractExecutable, Hash, LedgerEntryData, ScAddress, ScVal};
use soroban_sdk::{testutils::EnvTestConfig, Address, Env, TryFromVal};

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
const ONE_USDC: u128 = 10_000_000;
const ABI_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_aquarius_wrapper.wasm");

#[test]
fn one_scenario_mixes_abi_client_and_dynamic_invoke_without_manual_capture() {
    let run = mainnet(POOL)
        .cache(CAPTURE)
        .offline()
        .run(price_moves)
        .unwrap();

    assert_eq!(run.cache_status(), CacheStatus::Hit);
    assert_eq!(run.fixture().report().final_replay_rpc_reads(), 0);
    assert_ne!(
        captured_root_wasm_hash(run.fixture()),
        <[u8; 32]>::from(Sha256::digest(ABI_WASM)),
        "the ABI source WASM must not replace the captured network executable",
    );
}

#[test]
fn incompatible_local_abi_fails_instead_of_replacing_network_wasm() {
    mainnet(POOL)
        .cache(CAPTURE)
        .offline()
        .run(|fork| {
            let pool_id = fork.contract(POOL);
            let incompatible = incompatible_abi::Client::new(fork.env(), &pool_id);
            assert!(incompatible.try_get().is_err());
        })
        .unwrap();
}

fn price_moves(fork: &ScenarioFork<'_>) {
    let env = fork.env();
    let pool_id = fork.contract(POOL);
    let usdc = fork.contract(USDC);
    let pool = pool_abi::Client::new(env, &pool_id);
    let user = Address::try_from_val(env, &ScAddress::Contract(Hash([0x4b; 32]).into())).unwrap();

    let before = pool.estimate_swap(&1, &0, &ONE_USDC);
    let reserves = pool.get_reserves();
    let amount = reserves.get(1).unwrap() / 10;
    let amount_i128 = i128::try_from(amount).unwrap();

    fork.mock_all_auths();
    let admin = fork.invoke::<Address>(&usdc, "admin", ());
    fork.invoke::<()>(&usdc, "mint", (user.clone(), amount_i128));
    assert_eq!(env.auths()[0].0, admin);

    let received = fork.invoke::<u128>(&pool_id, "swap", (user, 1_u32, 0_u32, amount, 0_u128));
    assert!(received > 0);

    let after = pool.estimate_swap(&1, &0, &ONE_USDC);
    assert_ne!(after, before);
    assert!(after < before);
}

fn captured_root_wasm_hash(fixture: &kanatoko::CapturedFixture) -> [u8; 32] {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    let root: ScAddress = (&Address::from_str(&env, fixture.root_contract())).into();
    fixture
        .frozen_fixture()
        .ledger_snapshot()
        .ledger_entries
        .iter()
        .find_map(|(_, (entry, _))| {
            let LedgerEntryData::ContractData(data) = &entry.data else {
                return None;
            };
            if data.contract != root {
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
        .expect("captured root must reference network WASM")
}
