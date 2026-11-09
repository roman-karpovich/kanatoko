#![cfg(all(feature = "capture", kanatoko_protocol_29_fixtures))]

use kanatoko::{
    AuthMode, AuthorizationTree, CandidateInstallMode, CapturedFixture, ExecutionMode,
    InvokeOutcome, InvokeRequest, Receipt, ReceiptDisposition, StrictFork, StrictForkError,
    SUPPORTED_PROTOCOL_VERSION,
};
use soroban_env_host::xdr::{
    ContractId, Hash, Int128Parts, InvokeContractArgs, ScAddress, ScSymbol, ScVal,
    SorobanAuthorizedFunction, SorobanAuthorizedInvocation,
};
use soroban_sdk::{testutils::EnvTestConfig, Address, Env};

const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const CAPTURE: &str = "fixtures/mainnet/soroswap-xlm-usdc/capture.json";
const ROUTER: &str = "CAG5LRYQ5JVEUI5TEID72EYOVX44TTUJT5BQR2J6J77FH65PCCFAJDDH";
const PAIR: &str = "CAM7DY53G63XA4AJRS24Z6VFYAFSSF76C3RZ45BE5YU3FQS5255OOABP";
const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const ONE_USDC: i128 = 10_000_000;
const DEADLINE: u64 = u64::MAX;
const SWAP: &str = "swap_exact_tokens_for_tokens";
const WRAPPER_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_soroswap_wrapper.wasm");
const WRAPPER_SHA256: [u8; 32] = [
    0x1e, 0x7b, 0xca, 0x07, 0xc5, 0xaa, 0x5c, 0x2f, 0x31, 0x54, 0x48, 0x57, 0x02, 0x77, 0x99, 0x28,
    0x5d, 0xff, 0x97, 0x26, 0x6e, 0xd7, 0x89, 0x5c, 0xf2, 0x1d, 0x0e, 0x67, 0x36, 0xdc, 0xb3, 0xa8,
];
/// Quotes of the committed Protocol 29 capture on the Protocol 29 Host.
const PROTOCOL_29_QUOTES: Quotes = Quotes {
    before: 67_870_832,
    after: 56_106_992,
    restored: 67_870_832,
};

#[test]
fn candidate_calls_captured_soroswap_graph_statefully_with_receipts_and_revert() {
    let captured = CapturedFixture::from_file(CAPTURE, MAINNET_PASSPHRASE).unwrap();
    assert_eq!(captured.provenance().ledger_sequence(), 65_389_557);
    assert_eq!(captured.provenance().protocol_version(), 29);
    assert_eq!(captured.report().final_replay_rpc_reads(), 0);

    assert_eq!(candidate_scenario(&captured), PROTOCOL_29_QUOTES);
}

/// On a capture at the Host's own protocol the newer-protocol opt-in changes
/// nothing: same protocols, same digest, same quotes.
#[test]
fn newer_protocol_opt_in_runs_a_current_protocol_capture_unchanged() {
    let captured =
        CapturedFixture::from_file_allowing_newer_protocol(CAPTURE, MAINNET_PASSPHRASE).unwrap();
    assert_eq!(captured.provenance().ledger_sequence(), 65_389_557);
    assert_eq!(
        captured.provenance().protocol_version(),
        SUPPORTED_PROTOCOL_VERSION
    );
    assert_eq!(
        captured.provenance().executed_protocol_version(),
        SUPPORTED_PROTOCOL_VERSION
    );
    assert_eq!(
        hex(captured.frozen_fixture().ledger_digest()),
        "54b45ed4b3d3037e0882c004629ff7574c822d915f4f51e664f2e5f15b0f3ab5"
    );
    assert_eq!(captured.report().final_replay_rpc_reads(), 0);

    assert_eq!(candidate_scenario(&captured), PROTOCOL_29_QUOTES);
}

#[derive(Debug, Eq, PartialEq)]
struct Quotes {
    before: i128,
    after: i128,
    restored: i128,
}

#[allow(clippy::too_many_lines)]
fn candidate_scenario(captured: &CapturedFixture) -> Quotes {
    let mut fork = captured.fork();
    assert_eq!(
        fork.network_protocol_version(),
        captured.provenance().protocol_version()
    );
    assert_eq!(
        fork.executed_protocol_version(),
        captured.provenance().executed_protocol_version()
    );
    assert_eq!(
        fork.ledger_digest().unwrap(),
        captured.frozen_fixture().ledger_digest()
    );
    let router = address(ROUTER);
    let pair = address(PAIR);
    let candidate = ScAddress::Contract(ContractId(Hash([0x6c; 32])));
    let user = ScAddress::Contract(ContractId(Hash([0x4b; 32])));

    let registration = fork
        .register_candidate(
            candidate.clone(),
            WRAPPER_WASM,
            WRAPPER_SHA256,
            vec![ScVal::Address(router.clone()), ScVal::Address(pair.clone())],
        )
        .unwrap();
    assert_eq!(registration.mode, CandidateInstallMode::LocalInjection);
    assert_eq!(registration.upstream_reads, 0);
    let checkpoint = fork.checkpoint();

    let token_0 = invoke_success(
        &mut fork,
        call(&candidate, "token_0", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    assert_eq!(result(&token_0), &ScVal::Address(address(XLM)));
    let token_1 = invoke_success(
        &mut fork,
        call(&candidate, "token_1", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    assert_eq!(result(&token_1), &ScVal::Address(address(USDC)));

    let quote_before = quote(&mut fork, &candidate);
    assert!(quote_before > 0);
    let digest_after_quote = fork.ledger_digest().unwrap();
    let reserves_before_receipt = invoke_success(
        &mut fork,
        call(&candidate, "get_reserves", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    let reserves_before = sc_i128_vec(result(&reserves_before_receipt));
    assert_eq!(reserves_before.len(), 2);
    assert_eq!(fork.ledger_digest().unwrap(), digest_after_quote);
    let minted = reserves_before[1] / 10;
    assert!(minted > ONE_USDC);

    let mint_receipt = invoke_success(
        &mut fork,
        InvokeRequest {
            contract: address(USDC),
            function: symbol("mint"),
            args: vec![ScVal::Address(user.clone()), sc_i128(minted)],
        },
        ExecutionMode::Apply,
        AuthMode::Record,
    );
    assert_eq!(mint_receipt.disposition, ReceiptDisposition::Committed);
    assert!(!mint_receipt.authorization.is_empty());
    assert!(!mint_receipt.events.is_empty());
    assert!(!mint_receipt.state_changes.is_empty());
    assert_eq!(mint_receipt.upstream_reads, 0);
    assert_eq!(token_balance(&mut fork, USDC, &user), minted);
    assert_eq!(token_balance(&mut fork, XLM, &user), 0);

    let swap_request = call(
        &candidate,
        SWAP,
        vec![
            sc_i128(minted),
            sc_i128(0),
            usdc_to_xlm_path(),
            ScVal::Address(user.clone()),
            ScVal::U64(DEADLINE),
        ],
    );
    let digest_before_swap = fork.ledger_digest().unwrap();
    let preview = invoke_success(
        &mut fork,
        swap_request.clone(),
        ExecutionMode::Preview,
        AuthMode::Record,
    );
    assert_eq!(preview.disposition, ReceiptDisposition::Previewed);
    assert_ne!(preview.before_digest, preview.after_digest);
    assert!(!preview.state_changes.is_empty());
    assert_eq!(fork.ledger_digest().unwrap(), digest_before_swap);
    assert_candidate_router_sac_auth(&preview.authorization, &candidate, &router, &address(USDC));

    let swap = invoke_success(
        &mut fork,
        swap_request,
        ExecutionMode::Apply,
        AuthMode::MockExact(preview.authorization.clone()),
    );
    let received_xlm = sc_i128_vec(result(&swap))[1];
    assert!(received_xlm > 0);
    assert_eq!(swap.authorization, preview.authorization);
    assert_eq!(swap.disposition, ReceiptDisposition::Committed);
    assert!(!swap.events.is_empty());
    assert!(!swap.diagnostics.is_empty());
    assert!(!swap.state_changes.is_empty());
    assert!(swap
        .events
        .iter()
        .any(|event| { event.event.contract_id == contract_id(&pair) && !event.failed_call }));
    assert_ne!(fork.ledger_digest().unwrap(), digest_before_swap);
    assert_eq!(token_balance(&mut fork, USDC, &user), 0);
    assert_eq!(token_balance(&mut fork, XLM, &user), received_xlm);

    let reserves_after_receipt = invoke_success(
        &mut fork,
        call(&candidate, "get_reserves", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    let reserves_after = sc_i128_vec(result(&reserves_after_receipt));
    assert_eq!(reserves_after[0], reserves_before[0] - received_xlm);
    assert_eq!(reserves_after[1], reserves_before[1] + minted);
    let quote_after = quote(&mut fork, &candidate);
    assert_ne!(quote_after, quote_before);
    assert!(quote_after < quote_before);

    fork.revert(checkpoint).unwrap();
    let quote_restored = quote(&mut fork, &candidate);
    assert_eq!(quote_restored, quote_before);
    assert_eq!(token_balance(&mut fork, USDC, &user), 0);
    assert_eq!(token_balance(&mut fork, XLM, &user), 0);
    assert_unknown(&mut fork);
    assert_eq!(fork.upstream_reads(), 0);
    assert!(fork
        .receipts()
        .iter()
        .all(|receipt| receipt.upstream_reads == 0));
    assert_eq!(
        fork.network_protocol_version(),
        captured.provenance().protocol_version()
    );
    assert_eq!(
        fork.executed_protocol_version(),
        captured.provenance().executed_protocol_version()
    );

    Quotes {
        before: quote_before,
        after: quote_after,
        restored: quote_restored,
    }
}

fn hex(bytes: [u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(64);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}

fn quote(fork: &mut StrictFork, candidate: &ScAddress) -> i128 {
    let receipt = invoke_success(
        fork,
        call(
            candidate,
            "router_get_amounts_out",
            vec![sc_i128(ONE_USDC), usdc_to_xlm_path()],
        ),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    sc_i128_vec(result(&receipt))[1]
}

fn usdc_to_xlm_path() -> ScVal {
    let path = vec![ScVal::Address(address(USDC)), ScVal::Address(address(XLM))];
    ScVal::Vec(Some(path.try_into().unwrap()))
}

fn token_balance(fork: &mut StrictFork, token: &str, user: &ScAddress) -> i128 {
    let receipt = invoke_success(
        fork,
        InvokeRequest {
            contract: address(token),
            function: symbol("balance"),
            args: vec![ScVal::Address(user.clone())],
        },
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    let ScVal::I128(parts) = result(&receipt) else {
        panic!("token balance must be i128")
    };
    (i128::from(parts.hi) << 64) | i128::from(parts.lo)
}

fn invoke_success(
    fork: &mut StrictFork,
    request: InvokeRequest,
    execution: ExecutionMode,
    auth: AuthMode,
) -> Receipt {
    let receipt = fork.invoke(request, execution, auth).unwrap();
    assert!(receipt.outcome.is_success(), "{:#?}", receipt.outcome);
    assert_eq!(receipt.upstream_reads, 0);
    receipt
}

fn result(receipt: &Receipt) -> &ScVal {
    let InvokeOutcome::Success(value) = &receipt.outcome else {
        panic!("receipt must be successful")
    };
    value
}

fn assert_candidate_router_sac_auth(
    auth: &[AuthorizationTree],
    candidate: &ScAddress,
    router: &ScAddress,
    usdc: &ScAddress,
) {
    assert_eq!(auth.len(), 1);
    assert_contract_call(&auth[0].invocation, candidate, SWAP);
    let wrapper_children = &auth[0].invocation.sub_invocations;
    assert_eq!(wrapper_children.len(), 1);
    assert_contract_call(&wrapper_children[0], router, SWAP);
    let router_children = &wrapper_children[0].sub_invocations;
    assert_eq!(router_children.len(), 1);
    assert_contract_call(&router_children[0], usdc, "transfer");
}

fn assert_contract_call(
    invocation: &SorobanAuthorizedInvocation,
    expected_contract: &ScAddress,
    expected_function: &str,
) {
    let SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
        contract_address,
        function_name,
        ..
    }) = &invocation.function
    else {
        panic!("authorization node must be a contract call")
    };
    assert_eq!(contract_address, expected_contract);
    assert_eq!(function_name, &symbol(expected_function));
}

fn assert_unknown(fork: &mut StrictFork) {
    let before = fork.ledger_digest().unwrap();
    let error = fork
        .invoke(
            InvokeRequest {
                contract: ScAddress::Contract(ContractId(Hash([0xee; 32]))),
                function: symbol("missing"),
                args: vec![],
            },
            ExecutionMode::Apply,
            AuthMode::Enforce(vec![]),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        StrictForkError::UnknownLedgerKeys { count, .. } if count >= 1
    ));
    assert_eq!(fork.ledger_digest().unwrap(), before);
}

fn call(contract: &ScAddress, function: &str, args: Vec<ScVal>) -> InvokeRequest {
    InvokeRequest {
        contract: contract.clone(),
        function: symbol(function),
        args,
    }
}

fn sc_vec(value: &ScVal) -> Vec<ScVal> {
    let ScVal::Vec(Some(values)) = value else {
        panic!("value must be a vector")
    };
    values.0.to_vec()
}

fn sc_i128_vec(value: &ScVal) -> Vec<i128> {
    sc_vec(value).iter().map(sc_i128_value).collect()
}

fn sc_i128_value(value: &ScVal) -> i128 {
    let ScVal::I128(parts) = value else {
        panic!("value must be i128")
    };
    (i128::from(parts.hi) << 64) | i128::from(parts.lo)
}

fn sc_i128(value: i128) -> ScVal {
    let bytes = value.to_be_bytes();
    ScVal::I128(Int128Parts {
        hi: i64::from_be_bytes(bytes[..8].try_into().unwrap()),
        lo: u64::from_be_bytes(bytes[8..].try_into().unwrap()),
    })
}

fn address(value: &str) -> ScAddress {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    ScAddress::from(&Address::from_str(&env, value))
}

fn contract_id(address: &ScAddress) -> Option<ContractId> {
    match address {
        ScAddress::Contract(id) => Some(id.clone()),
        _ => None,
    }
}

fn symbol(value: &str) -> ScSymbol {
    ScSymbol(value.try_into().unwrap())
}
