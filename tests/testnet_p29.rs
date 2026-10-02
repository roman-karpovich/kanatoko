#![cfg(all(feature = "capture", kanatoko_protocol_29_fixtures))]

use std::fmt::Write as _;

use kanatoko::{testnet, CacheStatus, ScenarioFork};
use soroban_env_host::xdr::{ContractExecutable, LedgerEntryData, ScAddress, ScVal};
use soroban_sdk::{testutils::EnvTestConfig, Address, Env};

const CAPTURE: &str = "fixtures/testnet/native-xlm-p29/auto-capture.json";
const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";
const XLM: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";

mod stateful {
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
        sha256 = "0156a9a840e4147732fcf0479846220840ae4b4281c58354aa31cf70daf6b2ea",
    );
}

mod aquarius_wrapper {
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_aquarius_wrapper.wasm",
        sha256 = "e4d626c4960bc879dd44c8243c04b890a9bb0ddb7e1392d84a5b651c15aaab4f",
    );
}

#[test]
fn frozen_protocol_29_testnet_replays_offline() {
    let run = testnet().cache(CAPTURE).offline().run(scenario).unwrap();
    let fixture = run.fixture();

    assert_eq!(run.cache_status(), CacheStatus::Hit);
    assert_eq!(fixture.provenance().protocol_version(), 29);
    assert_eq!(
        fixture.provenance().network_passphrase(),
        TESTNET_PASSPHRASE
    );
    assert_eq!(fixture.provenance().ledger_sequence(), 4_983_918);
    assert_eq!(
        fixture.provenance().ledger_hash(),
        "cef7754b3c119c835fe69e9071ea0e1f1f4de26b854195e379c1dd5477f573b1"
    );
    assert_eq!(fixture.report().present_entries(), 1);
    assert_eq!(fixture.report().absent_entries(), 4);
    assert_eq!(fixture.report().final_replay_rpc_reads(), 0);
    assert_eq!(
        hex(fixture.frozen_fixture().ledger_digest()),
        "d28f50e6342075951ba9b082afc5e892eb42fc901ea0b44bfa7dc7cf55443c4a"
    );
    assert_eq!(
        hex(fixture.report().inventory_digest()),
        "66c7b6670b560c5a64731db2b0c1d99705c0a46c01313676ad99319073219967"
    );
    assert_native_xlm_is_a_stellar_asset_contract(fixture);
}

#[test]
#[ignore = "manual read-only Protocol 29 testnet fixture refresh"]
fn refresh_protocol_29_testnet_fixture() {
    let run = testnet().cache(CAPTURE).refresh().run(scenario).unwrap();

    assert!(matches!(
        run.cache_status(),
        CacheStatus::Created | CacheStatus::Refreshed
    ));
    assert_eq!(run.fixture().provenance().protocol_version(), 29);
    assert_eq!(run.fixture().report().final_replay_rpc_reads(), 0);
}

fn scenario(fork: &ScenarioFork<'_>) {
    let xlm = fork.contract(XLM);
    assert_eq!(fork.invoke::<u32>(&xlm, "decimals", ()), 7);

    let sender = fork.local_account("protocol-29-sender");
    let receiver = fork.local_account("protocol-29-receiver");
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

    let wrapper = fork.deploy(aquarius_wrapper::WASM, (candidate,));
    assert_eq!(
        aquarius_wrapper::Client::new(fork.env(), &wrapper).estimate_swap(&1, &0, &1_000),
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
