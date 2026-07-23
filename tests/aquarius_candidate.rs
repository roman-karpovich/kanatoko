#![cfg(all(feature = "capture", kanatoko_protocol_27_fixtures))]

use kanatoko::{
    AuthMode, AuthorizationTree, CandidateInstallMode, CapturedFixture, ExecutionMode,
    InvokeOutcome, InvokeRequest, Receipt, ReceiptDisposition, StrictFork, StrictForkError,
};
use soroban_env_host::xdr::{
    ContractId, Hash, Int128Parts, InvokeContractArgs, ScAddress, ScSymbol, ScVal,
    SorobanAuthorizedFunction, SorobanAuthorizedInvocation, UInt128Parts,
};
use soroban_sdk::{testutils::EnvTestConfig, Address, Env};

const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const CAPTURE: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/capture.json";
const POOL: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const ONE_USDC: u128 = 10_000_000;
const WRAPPER_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_aquarius_wrapper.wasm");
const WRAPPER_SHA256: [u8; 32] = [
    0x79, 0x8c, 0x95, 0x9e, 0x1e, 0x22, 0x09, 0x3c, 0x49, 0xb4, 0xec, 0x66, 0x36, 0xaa, 0xfe, 0xd1,
    0x4e, 0x88, 0x96, 0x14, 0xfb, 0x24, 0x34, 0x26, 0xab, 0xe5, 0x02, 0x3b, 0x30, 0xc1, 0x75, 0x20,
];

#[test]
#[allow(clippy::too_many_lines)]
fn candidate_calls_captured_aquarius_graph_statefully_with_receipts_and_revert() {
    let captured = CapturedFixture::from_file(CAPTURE, MAINNET_PASSPHRASE).unwrap();
    assert_eq!(captured.provenance().ledger_sequence(), 63_600_296);
    assert_eq!(captured.report().final_replay_rpc_reads(), 0);
    let mut fork = captured.fork();
    let pool = address(POOL);
    let candidate = ScAddress::Contract(ContractId(Hash([0x6c; 32])));
    let user = ScAddress::Contract(ContractId(Hash([0x4b; 32])));

    let registration = fork
        .register_candidate(
            candidate.clone(),
            WRAPPER_WASM,
            WRAPPER_SHA256,
            vec![ScVal::Address(pool.clone())],
        )
        .unwrap();
    assert_eq!(registration.mode, CandidateInstallMode::LocalInjection);
    assert_eq!(registration.upstream_reads, 0);
    let checkpoint = fork.checkpoint();

    let tokens = invoke_success(
        &mut fork,
        call(&candidate, "get_tokens", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    let tokens = sc_vec(result(&tokens));
    assert_eq!(
        tokens,
        vec![ScVal::Address(address(XLM)), ScVal::Address(address(USDC))]
    );

    let quote_before = quote(&mut fork, &candidate);
    assert!(quote_before > 0);
    let reserves_before_receipt = invoke_success(
        &mut fork,
        call(&candidate, "get_reserves", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    let reserves_before = sc_u128_vec(result(&reserves_before_receipt));
    assert_eq!(reserves_before.len(), 2);
    let minted = reserves_before[1] / 10;
    assert!(minted > ONE_USDC);
    let minted_i128 = i128::try_from(minted).unwrap();

    let mint_receipt = invoke_success(
        &mut fork,
        InvokeRequest {
            contract: address(USDC),
            function: symbol("mint"),
            args: vec![ScVal::Address(user.clone()), sc_i128(minted_i128)],
        },
        ExecutionMode::Apply,
        AuthMode::Record,
    );
    assert_eq!(mint_receipt.disposition, ReceiptDisposition::Committed);
    assert!(!mint_receipt.authorization.is_empty());
    assert!(!mint_receipt.events.is_empty());
    assert!(!mint_receipt.state_changes.is_empty());
    assert_eq!(mint_receipt.upstream_reads, 0);
    assert_eq!(token_balance(&mut fork, USDC, &user), minted_i128);
    assert_eq!(token_balance(&mut fork, XLM, &user), 0);

    let swap_request = call(
        &candidate,
        "swap",
        vec![
            ScVal::Address(user.clone()),
            ScVal::U32(1),
            ScVal::U32(0),
            sc_u128(minted),
            sc_u128(0),
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
    assert_candidate_pool_sac_auth(&preview.authorization, &candidate, &pool, &address(USDC));

    let swap = invoke_success(
        &mut fork,
        swap_request,
        ExecutionMode::Apply,
        AuthMode::MockExact(preview.authorization.clone()),
    );
    let received_xlm = sc_u128_value(result(&swap));
    assert!(received_xlm > 0);
    assert_eq!(swap.authorization, preview.authorization);
    assert_eq!(swap.disposition, ReceiptDisposition::Committed);
    assert!(!swap.events.is_empty());
    assert!(!swap.diagnostics.is_empty());
    assert!(!swap.state_changes.is_empty());
    assert!(swap
        .events
        .iter()
        .any(|event| { event.event.contract_id == contract_id(&pool) && !event.failed_call }));
    assert_ne!(fork.ledger_digest().unwrap(), digest_before_swap);
    assert_eq!(token_balance(&mut fork, USDC, &user), 0);
    assert_eq!(
        token_balance(&mut fork, XLM, &user),
        i128::try_from(received_xlm).unwrap()
    );

    let reserves_after_receipt = invoke_success(
        &mut fork,
        call(&candidate, "get_reserves", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    let reserves_after = sc_u128_vec(result(&reserves_after_receipt));
    assert_eq!(reserves_after[0], reserves_before[0] - received_xlm);
    assert!(reserves_after[1] > reserves_before[1]);
    let quote_after = quote(&mut fork, &candidate);
    assert_ne!(quote_after, quote_before);
    assert!(quote_after < quote_before);

    fork.revert(checkpoint).unwrap();
    assert_eq!(quote(&mut fork, &candidate), quote_before);
    assert_eq!(token_balance(&mut fork, USDC, &user), 0);
    assert_eq!(token_balance(&mut fork, XLM, &user), 0);
    assert_unknown(&mut fork);
    assert_eq!(fork.upstream_reads(), 0);
    assert!(fork
        .receipts()
        .iter()
        .all(|receipt| receipt.upstream_reads == 0));
}

fn quote(fork: &mut StrictFork, candidate: &ScAddress) -> u128 {
    let receipt = invoke_success(
        fork,
        call(
            candidate,
            "estimate_swap",
            vec![ScVal::U32(1), ScVal::U32(0), sc_u128(ONE_USDC)],
        ),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    );
    sc_u128_value(result(&receipt))
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

fn assert_candidate_pool_sac_auth(
    auth: &[AuthorizationTree],
    candidate: &ScAddress,
    pool: &ScAddress,
    usdc: &ScAddress,
) {
    assert_eq!(auth.len(), 1);
    assert_contract_call(&auth[0].invocation, candidate, "swap");
    let wrapper_children = &auth[0].invocation.sub_invocations;
    assert_eq!(wrapper_children.len(), 1);
    assert_contract_call(&wrapper_children[0], pool, "swap");
    let pool_children = &wrapper_children[0].sub_invocations;
    assert_eq!(pool_children.len(), 1);
    assert_contract_call(&pool_children[0], usdc, "transfer");
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

fn sc_u128_vec(value: &ScVal) -> Vec<u128> {
    sc_vec(value).iter().map(sc_u128_value).collect()
}

fn sc_u128_value(value: &ScVal) -> u128 {
    let ScVal::U128(parts) = value else {
        panic!("value must be u128")
    };
    (u128::from(parts.hi) << 64) | u128::from(parts.lo)
}

fn sc_u128(value: u128) -> ScVal {
    let bytes = value.to_be_bytes();
    ScVal::U128(UInt128Parts {
        hi: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
        lo: u64::from_be_bytes(bytes[8..].try_into().unwrap()),
    })
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
