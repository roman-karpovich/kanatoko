#![cfg(all(feature = "capture", kanatoko_protocol_30_fixtures))]

use std::fmt::Write as _;

use kanatoko::{
    AppliedAuthMode, AuthMode, CaptureBuilder, CapturedFixture, ExecutionMode, InvokeOutcome,
    InvokeRequest, ReceiptDisposition, SUPPORTED_PROTOCOL_VERSION,
};
use soroban_env_host::xdr::{
    ContractEventBody, ContractExecutable, ContractId, Hash, Int128Parts, LedgerEntryData,
    MuxedContract, ScAddress, ScMap, ScMapEntry, ScSymbol, ScVal,
};
use soroban_sdk::{
    testutils::{EnvTestConfig, MuxedAddress as _},
    token::TokenClient,
    Address, Env, MuxedAddress,
};

const CAPTURE: &str = "fixtures/standalone/native-xlm-stateful-p30/capture.json";
const STANDALONE_PASSPHRASE: &str = "Standalone Network ; February 2017";
const LOCAL_RPC: &str = "http://localhost:8000/rpc";
const XLM: &str = "CDMLFMKMMD7MWZP3FKUBZPVHTUEDLSX4BYGYKH4GCESXYHS3IHQ4EIG4";
const DEPLOYER: &str = "GDG2BXH36BVGBOF62IXSAIZXKJ62CAIWHEFQOAA5ZGIEOMXQDYSPZ7VT";
const STATEFUL: &str = "CCNHIRURTVMTX65DOASMBGJSQZLZMFGRO7QBHMBDSFPGEQ7AWH6VMTH5";
const STATEFUL_WASM_SHA256: &str =
    "0156a9a840e4147732fcf0479846220840ae4b4281c58354aa31cf70daf6b2ea";
const MUXED_ID: u64 = 30;
const LEDGER_SEQUENCE: u32 = 318;
const LEDGER_HASH: &str = "31f741303e186fde9fba0fd54fb6e5d161abd9fcac1effab3935c94d263859d9";
const PRESENT_ENTRIES: usize = 5;
const ABSENT_ENTRIES: usize = 2;
const LEDGER_DIGEST: &str = "046c76b43dd29f19683c865d6116ea14c41d957dc6c3f2c7e281d8d3b0509998";
const INVENTORY_DIGEST: &str = "2ae27c8eae93cdce78b0f689ff1cbe25836827a80ca48cfa894e3ac757215ca5";

mod stateful {
    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
        sha256 = "0156a9a840e4147732fcf0479846220840ae4b4281c58354aa31cf70daf6b2ea",
    );
}

#[test]
fn frozen_protocol_30_standalone_replays_offline() {
    let captured = CapturedFixture::from_file(CAPTURE, STANDALONE_PASSPHRASE).unwrap();

    assert_eq!(captured.provenance().protocol_version(), 30);
    assert_eq!(
        captured.provenance().executed_protocol_version(),
        SUPPORTED_PROTOCOL_VERSION
    );
    assert_eq!(
        captured.provenance().network_passphrase(),
        STANDALONE_PASSPHRASE
    );
    assert_eq!(captured.provenance().ledger_sequence(), LEDGER_SEQUENCE);
    assert_eq!(captured.provenance().ledger_hash(), LEDGER_HASH);
    assert_eq!(captured.report().present_entries(), PRESENT_ENTRIES);
    assert_eq!(captured.report().absent_entries(), ABSENT_ENTRIES);
    assert_eq!(captured.report().final_replay_rpc_reads(), 0);
    assert_eq!(
        hex(captured.frozen_fixture().ledger_digest()),
        LEDGER_DIGEST
    );
    assert_eq!(hex(captured.report().inventory_digest()), INVENTORY_DIGEST);
    assert!(matches!(
        instance_executable(&captured, XLM),
        ContractExecutable::StellarAsset
    ));
    // The stateful fixture was uploaded and deployed by real Protocol 30
    // transactions; its captured code is the committed artifact.
    let ContractExecutable::Wasm(Hash(code)) = instance_executable(&captured, STATEFUL) else {
        panic!("captured stateful instance must reference WASM");
    };
    assert_eq!(hex(code), STATEFUL_WASM_SHA256);

    captured.replay(scenario).unwrap();
}

#[test]
fn protocol_30_capture_loads_unchanged_with_the_newer_protocol_opt_in() {
    let captured =
        CapturedFixture::from_file_allowing_newer_protocol(CAPTURE, STANDALONE_PASSPHRASE).unwrap();

    assert_eq!(captured.provenance().protocol_version(), 30);
    assert_eq!(captured.provenance().executed_protocol_version(), 30);
    captured.replay(scenario).unwrap();
}

/// CAP-84: a native XLM transfer to a muxed contract address credits the
/// contract and reports the muxed id in the transfer event.
#[test]
fn protocol_30_strict_fork_transfers_to_a_muxed_contract_address() {
    let captured = CapturedFixture::from_file(CAPTURE, STANDALONE_PASSPHRASE).unwrap();
    let mut fork = captured.fork();
    assert_eq!(fork.network_protocol_version(), 30);
    assert_eq!(fork.executed_protocol_version(), 30);
    let initial_digest = fork.ledger_digest().unwrap();
    let checkpoint = fork.checkpoint();

    let get = fork
        .invoke(
            request(STATEFUL, "get", vec![]),
            ExecutionMode::Preview,
            AuthMode::Enforce(vec![]),
        )
        .unwrap();
    assert_eq!(get.outcome, InvokeOutcome::Success(ScVal::I64(42)));

    let increment = fork
        .invoke(
            request(STATEFUL, "increment", vec![ScVal::I64(1)]),
            ExecutionMode::Apply,
            AuthMode::Enforce(vec![]),
        )
        .unwrap();
    assert_eq!(increment.outcome, InvokeOutcome::Success(ScVal::I64(43)));
    assert_eq!(increment.disposition, ReceiptDisposition::Committed);
    assert_eq!(increment.events.len(), 1);

    let muxed = ScAddress::MuxedContract(MuxedContract {
        id: MUXED_ID,
        contract_id: contract_id(STATEFUL),
    });
    let transfer = fork
        .invoke(
            request(
                XLM,
                "transfer",
                vec![
                    ScVal::Address(address(DEPLOYER)),
                    ScVal::Address(muxed),
                    i128(1),
                ],
            ),
            ExecutionMode::Apply,
            AuthMode::Record,
        )
        .unwrap();
    assert!(transfer.outcome.is_success(), "{:?}", transfer.outcome);
    assert_eq!(transfer.auth_mode, AppliedAuthMode::RecordMockSatisfied);
    assert_eq!(transfer.disposition, ReceiptDisposition::Committed);
    assert_eq!(transfer.authorization.len(), 1);
    assert_eq!(transfer.authorization[0].address, address(DEPLOYER));
    let [event] = transfer.events.as_slice() else {
        panic!("one transfer event expected: {:?}", transfer.events);
    };
    let ContractEventBody::V0(body) = &event.event.body;
    assert_eq!(body.topics[2], ScVal::Address(address(STATEFUL)));
    assert_eq!(
        body.data,
        ScVal::Map(Some(
            ScMap::sorted_from(vec![
                ScMapEntry {
                    key: ScVal::Symbol(symbol("amount")),
                    val: i128(1),
                },
                ScMapEntry {
                    key: ScVal::Symbol(symbol("to_muxed_id")),
                    val: ScVal::U64(MUXED_ID),
                },
            ])
            .unwrap()
        ))
    );

    let balance = fork
        .invoke(
            request(XLM, "balance", vec![ScVal::Address(address(STATEFUL))]),
            ExecutionMode::Preview,
            AuthMode::Enforce(vec![]),
        )
        .unwrap();
    assert_eq!(balance.outcome, InvokeOutcome::Success(i128(1)));

    fork.revert(checkpoint).unwrap();
    assert_eq!(fork.ledger_digest().unwrap(), initial_digest);
    assert_eq!(fork.upstream_reads(), 0);
    assert!(fork
        .receipts()
        .iter()
        .all(|receipt| receipt.upstream_reads == 0));
}

#[test]
#[ignore = "manual read-only capture from a local Protocol 30 quickstart network"]
fn refresh_protocol_30_standalone_fixture() {
    let captured = CaptureBuilder::rpc(LOCAL_RPC, STANDALONE_PASSPHRASE)
        .unwrap()
        .capture(scenario)
        .unwrap();

    assert_eq!(captured.provenance().protocol_version(), 30);
    assert_eq!(captured.report().final_replay_rpc_reads(), 0);
    captured.write_file(CAPTURE).unwrap();
    let written = CapturedFixture::from_file(CAPTURE, STANDALONE_PASSPHRASE).unwrap();
    println!(
        "ledger {} hash {} present {} absent {} ledger digest {} inventory digest {}",
        written.provenance().ledger_sequence(),
        written.provenance().ledger_hash(),
        written.report().present_entries(),
        written.report().absent_entries(),
        hex(written.frozen_fixture().ledger_digest()),
        hex(written.report().inventory_digest()),
    );
}

fn scenario(env: &Env) {
    let xlm = TokenClient::new(env, &Address::from_str(env, XLM));
    assert_eq!(xlm.decimals(), 7);

    let contract = Address::from_str(env, STATEFUL);
    let stateful = stateful::Client::new(env, &contract);
    // Constructed with 41 and incremented once on the network.
    assert_eq!(stateful.get(), 42);
    assert_eq!(stateful.increment(&1), 43);

    let deployer = Address::from_str(env, DEPLOYER);
    let deployer_before = xlm.balance(&deployer);
    assert!(deployer_before > 0);
    assert_eq!(xlm.balance(&contract), 0);

    env.mock_all_auths();
    let muxed = MuxedAddress::new(contract.clone(), MUXED_ID);
    assert!(muxed.to_strkey().to_string().starts_with('W'));
    xlm.transfer(&deployer, &muxed, &1);
    assert_eq!(xlm.balance(&deployer), deployer_before - 1);
    assert_eq!(xlm.balance(&contract), 1);
}

fn instance_executable(captured: &CapturedFixture, contract: &str) -> ContractExecutable {
    let contract = address(contract);
    captured
        .frozen_fixture()
        .ledger_snapshot()
        .ledger_entries
        .iter()
        .find_map(|(_, (entry, _))| {
            let LedgerEntryData::ContractData(data) = &entry.data else {
                return None;
            };
            if data.contract != contract || !matches!(data.key, ScVal::LedgerKeyContractInstance) {
                return None;
            }
            let ScVal::ContractInstance(instance) = &data.val else {
                return None;
            };
            Some(instance.executable.clone())
        })
        .expect("captured contract instance")
}

fn request(contract: &str, function: &str, args: Vec<ScVal>) -> InvokeRequest {
    InvokeRequest {
        contract: address(contract),
        function: symbol(function),
        args,
    }
}

fn address(value: &str) -> ScAddress {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    if value.starts_with('G') {
        ScAddress::from(&Address::from_str(&env, value))
    } else {
        ScAddress::Contract(contract_id(value))
    }
}

fn contract_id(value: &str) -> ContractId {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    let ScAddress::Contract(id) = ScAddress::from(&Address::from_str(&env, value)) else {
        panic!("{value} is not a contract address");
    };
    id
}

fn symbol(value: &str) -> ScSymbol {
    ScSymbol(value.try_into().unwrap())
}

fn i128(value: i64) -> ScVal {
    ScVal::I128(Int128Parts {
        hi: if value < 0 { -1 } else { 0 },
        lo: value.cast_unsigned(),
    })
}

fn hex(bytes: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}
