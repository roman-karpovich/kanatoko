#![cfg(all(feature = "capture", kanatoko_protocol_29_fixtures))]

use std::collections::BTreeMap;

use kanatoko::{mainnet, CacheStatus, PreviewAuth, ScenarioFork, SUPPORTED_PROTOCOL_VERSION};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    AlphaNum4, AssetCode4, ContractEventBody, ContractExecutable, Hash, LedgerEntry,
    LedgerEntryData, LedgerKey, ScAddress, ScVal, TrustLineAsset, TrustLineFlags,
};
use soroban_sdk::{
    testutils::{EnvTestConfig, Events as _, MuxedAddress as _},
    vec, Address, Env, MuxedAddress, Vec as SorobanVec,
};

mod pool_abi {
    #![allow(clippy::too_many_arguments)]

    // Deliberately import a different executable that exposes the same pair
    // and router methods. It generates Rust bindings only: calls target the
    // captured network addresses and execute the captured network WASM.
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_soroswap_wrapper.wasm",
        sha256 = "1e7bca07c5aa5c2f31544857027799285dff97266ed7895cf21d0e6736dcb3a8",
    );
}

mod incompatible_abi {
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
        sha256 = "0156a9a840e4147732fcf0479846220840ae4b4281c58354aa31cf70daf6b2ea",
    );
}

mod replacement {
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_legacy_v25_fixture.wasm",
        sha256 = "d601c7569be29b0a52af409ed65425b8c3595db8a83c444fe65dd8294423a879",
    );
}

const CAPTURE: &str = "fixtures/mainnet/soroswap-xlm-usdc/auto-capture.json";
const ROUTER: &str = "CAG5LRYQ5JVEUI5TEID72EYOVX44TTUJT5BQR2J6J77FH65PCCFAJDDH";
const PAIR: &str = "CAM7DY53G63XA4AJRS24Z6VFYAFSSF76C3RZ45BE5YU3FQS5255OOABP";
const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const USDC_ISSUER: &str = "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN";
const REAL_ACCOUNT: &str = "GAI2BF5I2G77J653SVF5ONB77SURJSD225MLZZOBWWBR3WU3YEPVIEO2";
const ONE_USDC: i128 = 10_000_000;
const DEADLINE: u64 = u64::MAX;
const MUXED_ID: u64 = 42;
const ABI_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_soroswap_wrapper.wasm");

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
        captured_contract_wasm_hash(run.fixture(), PAIR),
        <[u8; 32]>::from(Sha256::digest(ABI_WASM)),
        "the ABI source WASM must not replace the captured network executable",
    );
    assert_captured_real_account_state(run.fixture());
}

/// On a cache recorded at the Host's own protocol the newer-protocol opt-in
/// changes nothing: the network and executed protocols are both the Host's.
/// Acceptance of a ledger one protocol ahead is covered by the synthetic
/// in-memory tests in the library and in `tests/fixture_runtime.rs`.
#[test]
fn newer_protocol_opt_in_replays_the_current_protocol_cache_unchanged() {
    let run = mainnet()
        .cache(CAPTURE)
        .offline()
        .allow_newer_protocol()
        .run(|fork| {
            assert_eq!(fork.network_protocol_version(), SUPPORTED_PROTOCOL_VERSION);
            assert_eq!(fork.executed_protocol_version(), SUPPORTED_PROTOCOL_VERSION);
            let pair = fork.contract(PAIR);
            let report = fork
                .preview(&pair, "token_0", (), PreviewAuth::Record)
                .unwrap();
            assert!(report.receipt().outcome.is_success());
            assert_eq!(report.receipt().upstream_reads, 0);
            assert_eq!(
                report.network_protocol_version(),
                SUPPORTED_PROTOCOL_VERSION
            );
            assert_eq!(
                report.executed_protocol_version(),
                SUPPORTED_PROTOCOL_VERSION
            );
            full_scenario(fork);
        })
        .unwrap();

    assert_eq!(run.cache_status(), CacheStatus::Hit);
    let provenance = run.fixture().provenance();
    assert_eq!(provenance.ledger_sequence(), 65_389_638);
    assert_eq!(provenance.protocol_version(), 29);
    assert_eq!(provenance.executed_protocol_version(), 29);
    assert_eq!(run.fixture().report().final_replay_rpc_reads(), 0);
    assert_captured_real_account_state(run.fixture());
}

#[test]
#[ignore = "manual read-only Protocol 29 mainnet fixture refresh"]
fn refresh_protocol_29_mainnet_fixture() {
    let run = mainnet()
        .cache(CAPTURE)
        .refresh()
        .run(full_scenario)
        .unwrap();

    assert!(matches!(
        run.cache_status(),
        CacheStatus::Created | CacheStatus::Refreshed
    ));
    assert_eq!(run.fixture().provenance().protocol_version(), 29);
    assert_eq!(run.fixture().report().final_replay_rpc_reads(), 0);
    assert_captured_real_account_state(run.fixture());
}

#[test]
fn incompatible_local_abi_fails_instead_of_replacing_network_wasm() {
    mainnet()
        .cache(CAPTURE)
        .offline()
        .run(|fork| {
            let pair_id = fork.contract(PAIR);
            let incompatible = incompatible_abi::Client::new(fork.env(), &pair_id);
            assert!(incompatible.try_get().is_err());
        })
        .unwrap();
}

#[test]
fn replacement_wasm_runs_at_captured_address_without_changing_its_storage() {
    mainnet()
        .cache(CAPTURE)
        .offline()
        .run(|fork| {
            let pair = fork.contract(PAIR);
            let before = contract_storage(fork.env(), &pair);
            assert!(!before.is_empty());

            fork.replace_wasm(&pair, replacement::WASM);

            let replacement = replacement::Client::new(fork.env(), &pair);
            assert_eq!(replacement.sdk_major(), 25);
            assert_eq!(contract_storage(fork.env(), &pair), before);
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
    let router_id = fork.contract(ROUTER);
    let pair_id = fork.contract(PAIR);
    let usdc = fork.contract(USDC);
    let router = pool_abi::Client::new(env, &router_id);
    let pair = pool_abi::Client::new(env, &pair_id);
    assert!(matches!(ScAddress::from(&user), ScAddress::Account(_)));

    let path = vec![env, usdc.clone(), pair.token_0()];
    let before = router
        .router_get_amounts_out(&ONE_USDC, &path)
        .get(1)
        .unwrap();
    let (_, usdc_reserve) = pair.get_reserves();
    let amount = usdc_reserve / 10;

    fork.mock_all_auths();
    let admin = fork.invoke::<Address>(&usdc, "admin", ());
    fork.invoke::<()>(&usdc, "trust", (user.clone(),));
    fork.invoke::<()>(&usdc, "set_authorized", (user.clone(), true));
    fork.invoke::<()>(&usdc, "mint", (user.clone(), amount));
    assert_eq!(env.auths()[0].0, admin);

    let amounts = fork.invoke::<SorobanVec<i128>>(
        &router_id,
        "swap_exact_tokens_for_tokens",
        (amount, 0_i128, path.clone(), user, DEADLINE),
    );
    assert!(amounts.get(1).unwrap() > 0);

    let after = router
        .router_get_amounts_out(&ONE_USDC, &path)
        .get(1)
        .unwrap();
    assert_ne!(after, before);
    assert!(after < before);
}

fn real_account_and_muxed_destination(fork: &ScenarioFork<'_>) {
    let env = fork.env();
    let pair_id = fork.contract(PAIR);
    let pair = pool_abi::Client::new(env, &pair_id);
    let xlm = pair.token_0();
    let usdc = pair.token_1();
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

fn contract_storage(
    env: &Env,
    contract: &Address,
) -> BTreeMap<LedgerKey, (LedgerEntry, Option<u32>)> {
    let contract: ScAddress = contract.into();
    env.to_ledger_snapshot()
        .ledger_entries
        .into_iter()
        .filter_map(|(key, (entry, live_until))| {
            let LedgerEntryData::ContractData(data) = &entry.data else {
                return None;
            };
            if data.contract != contract {
                return None;
            }

            let mut entry = *entry;
            if let LedgerEntryData::ContractData(data) = &mut entry.data {
                if let ScVal::ContractInstance(instance) = &mut data.val {
                    instance.executable = ContractExecutable::Wasm(Hash([0; 32]));
                }
            }
            Some((*key, (entry, live_until)))
        })
        .collect()
}
