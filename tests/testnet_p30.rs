#![cfg(all(feature = "capture", kanatoko_protocol_30_fixtures))]

use std::fmt::Write as _;

use kanatoko::{testnet, CacheStatus, ScenarioFork};
use soroban_env_host::xdr::{ContractExecutable, LedgerEntryData, ScAddress, ScVal};
use soroban_sdk::{testutils::EnvTestConfig, Address, Env};

const CAPTURE: &str = "fixtures/testnet/native-xlm-p30/auto-capture.json";
const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";
const XLM: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";

mod stateful {
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
        sha256 = "76a511a1e7a1fff4a5fecb77f6d6a4018c9f090ff40a8a52b7233c44d71aa18e",
    );
}

mod quote_forwarder {
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_quote_forwarder.wasm",
        sha256 = "bafe532ffb80260d56a07cec4c30b3fa512e6403067a0552266ebbbc2d1a25ba",
    );
}

#[test]
fn frozen_protocol_30_testnet_replays_offline() {
    let run = testnet().cache(CAPTURE).offline().run(scenario).unwrap();
    let fixture = run.fixture();

    assert_eq!(run.cache_status(), CacheStatus::Hit);
    assert_eq!(fixture.provenance().protocol_version(), 30);
    assert_eq!(
        fixture.provenance().network_passphrase(),
        TESTNET_PASSPHRASE
    );
    assert_eq!(fixture.provenance().ledger_sequence(), 5_898_217);
    assert_eq!(
        fixture.provenance().ledger_hash(),
        "ed13d42c509e427da7954bb23a1f316d95b00d0a2c62b061c301e3552a3188c1"
    );
    assert_eq!(fixture.report().present_entries(), 1);
    assert_eq!(fixture.report().absent_entries(), 4);
    assert_eq!(fixture.report().final_replay_rpc_reads(), 0);
    assert_eq!(
        hex(fixture.frozen_fixture().ledger_digest()),
        "4f4868dd74bd4ef4a337b7ab99fa202a1dcaa809ec91f44503f9bd3ef6720d38"
    );
    assert_eq!(
        hex(fixture.report().inventory_digest()),
        "38845692ef455275b7e97469fe7615a21acfc7bf6bf57918646b2ba57ec89e97"
    );
    assert_native_xlm_is_a_stellar_asset_contract(fixture);
}

#[test]
#[ignore = "manual read-only Protocol 30 testnet fixture refresh"]
fn refresh_protocol_30_testnet_fixture() {
    let run = testnet().cache(CAPTURE).refresh().run(scenario).unwrap();

    assert!(matches!(
        run.cache_status(),
        CacheStatus::Created | CacheStatus::Refreshed
    ));
    assert_eq!(run.fixture().provenance().protocol_version(), 30);
    assert_eq!(run.fixture().report().final_replay_rpc_reads(), 0);
}

fn scenario(fork: &ScenarioFork<'_>) {
    let xlm = fork.contract(XLM);
    assert_eq!(fork.invoke::<u32>(&xlm, "decimals", ()), 7);

    let sender = fork.local_account("protocol-30-sender");
    let receiver = fork.local_account("protocol-30-receiver");
    fork.fund_local_account(&sender, 100_000_000);
    fork.fund_local_account(&receiver, 100_000_000);
    let sender_before = fork.invoke::<i128>(&xlm, "balance", (sender.clone(),));
    let receiver_before = fork.invoke::<i128>(&xlm, "balance", (receiver.clone(),));

    fork.mock_all_auths();
    fork.invoke::<()>(&xlm, "transfer", (sender.clone(), receiver.clone(), 1_i128));
    assert_eq!(
        fork.invoke::<i128>(&xlm, "balance", (sender,)),
        sender_before - 1
    );
    assert_eq!(
        fork.invoke::<i128>(&xlm, "balance", (receiver,)),
        receiver_before + 1
    );

    let candidate = fork.deploy(stateful::WASM, (41_i64,));
    assert_eq!(stateful::Client::new(fork.env(), &candidate).get(), 41);

    let wrapper = fork.deploy(quote_forwarder::WASM, (candidate,));
    assert_eq!(
        quote_forwarder::Client::new(fork.env(), &wrapper).estimate_swap(&1, &0, &1_000),
        1_007
    );
}

fn assert_native_xlm_is_a_stellar_asset_contract(fixture: &kanatoko::CapturedFixture) {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    let xlm = ScAddress::from(&Address::from_str(&env, XLM));
    let instance = fixture
        .frozen_fixture()
        .ledger_snapshot()
        .ledger_entries
        .iter()
        .find_map(|(_, (entry, _))| {
            let LedgerEntryData::ContractData(data) = &entry.data else {
                return None;
            };
            if data.contract != xlm || !matches!(data.key, ScVal::LedgerKeyContractInstance) {
                return None;
            }
            let ScVal::ContractInstance(instance) = &data.val else {
                return None;
            };
            Some(instance)
        })
        .expect("captured testnet XLM instance");

    assert!(matches!(
        instance.executable,
        ContractExecutable::StellarAsset
    ));
}

fn hex(bytes: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}
