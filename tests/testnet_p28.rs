#![cfg(all(feature = "capture", kanatoko_protocol_28_fixtures))]

use std::fmt::Write as _;

use kanatoko::{testnet, CacheStatus, ScenarioFork};
use soroban_env_host::xdr::{ContractExecutable, LedgerEntryData, ScAddress, ScVal};
use soroban_sdk::{testutils::EnvTestConfig, Address, Env};

const CAPTURE: &str = "fixtures/testnet/native-xlm-p28/auto-capture.json";
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
fn frozen_protocol_28_testnet_replays_offline() {
    let run = testnet().cache(CAPTURE).offline().run(scenario).unwrap();
    let fixture = run.fixture();

    assert_eq!(run.cache_status(), CacheStatus::Hit);
    assert_eq!(fixture.provenance().protocol_version(), 28);
    assert_eq!(
        fixture.provenance().network_passphrase(),
        TESTNET_PASSPHRASE
    );
    assert_eq!(fixture.provenance().ledger_sequence(), 4_414_462);
    assert_eq!(
        fixture.provenance().ledger_hash(),
        "e8956fc2ac30d2cad7be81bef50c52d7402dbd6564aeef9dfcefc7e909f5dcc1"
    );
    assert_eq!(fixture.report().present_entries(), 1);
    assert_eq!(fixture.report().absent_entries(), 4);
    assert_eq!(fixture.report().final_replay_rpc_reads(), 0);
    assert_eq!(
        hex(fixture.frozen_fixture().ledger_digest()),
        "d50c3438ba1a6db7fc57fc79b40343a6cba17aefdce6c04d735cb8897488ac97"
    );
    assert_eq!(
        hex(fixture.report().inventory_digest()),
        "3c365ed2d975598a08875290d5cb3e7e5551db13985eb2e9f76f6175adf3aff8"
    );
    assert_native_xlm_is_a_stellar_asset_contract(fixture);
}

#[test]
#[ignore = "manual read-only Protocol 28 testnet fixture refresh"]
fn refresh_protocol_28_testnet_fixture() {
    let run = testnet().cache(CAPTURE).refresh().run(scenario).unwrap();

    assert!(matches!(
        run.cache_status(),
        CacheStatus::Created | CacheStatus::Refreshed
    ));
    assert_eq!(run.fixture().provenance().protocol_version(), 28);
    assert_eq!(run.fixture().report().final_replay_rpc_reads(), 0);
}

fn scenario(fork: &ScenarioFork<'_>) {
    let xlm = fork.contract(XLM);
    assert_eq!(fork.invoke::<u32>(&xlm, "decimals", ()), 7);

    let sender = fork.local_account("protocol-28-sender");
    let receiver = fork.local_account("protocol-28-receiver");
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
