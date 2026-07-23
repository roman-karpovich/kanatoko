#![cfg(feature = "capture")]

use kanatoko::{
    AppliedAuthMode, AuthMode, CandidateInstallMode, CapturedFixture, ExecutionMode, InvokeOutcome,
    InvokeRequest, ReceiptDisposition, StrictForkError,
};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    ContractId, Hash, Int128Parts, InvokeContractArgs, LedgerKey, ScAddress, ScSymbol, ScVal,
    SorobanAddressCredentials, SorobanAuthorizationEntry, SorobanAuthorizedFunction,
    SorobanAuthorizedInvocation, SorobanCredentials, UInt128Parts,
};
use soroban_sdk::{testutils::EnvTestConfig, Address, Env};

const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const CAPTURE: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/capture.json";
const POOL: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const ONE_USDC: u64 = 10_000_000;
const STATEFUL_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_stateful_fixture.wasm");
const STATEFUL_WASM_SHA256: [u8; 32] = [
    0x6f, 0x6f, 0x46, 0x97, 0x98, 0xb6, 0x86, 0xcc, 0x48, 0x5a, 0xd2, 0x07, 0xf3, 0x2e, 0x3f, 0x77,
    0x00, 0x9c, 0x4b, 0x69, 0xab, 0x24, 0x37, 0xd9, 0xbd, 0xca, 0x97, 0xf1, 0x49, 0xb5, 0x4b, 0xa8,
];

#[test]
#[allow(clippy::too_many_lines)]
fn strict_mutable_fork_preserves_absent_unknown_and_atomic_calls_across_revert() {
    let captured = CapturedFixture::from_file(CAPTURE, MAINNET_PASSPHRASE).unwrap();
    let mut fork = captured.fork();
    let initial_digest = fork.ledger_digest().unwrap();
    let checkpoint = fork.checkpoint();
    let user = ScAddress::Contract(ContractId(Hash([0x4b; 32])));

    let quote = fork
        .invoke(
            request(
                POOL,
                "estimate_swap",
                vec![
                    ScVal::U32(1),
                    ScVal::U32(0),
                    ScVal::U128(UInt128Parts {
                        hi: 0,
                        lo: ONE_USDC,
                    }),
                ],
            ),
            ExecutionMode::Preview,
            AuthMode::Enforce(vec![]),
        )
        .unwrap();
    assert!(matches!(
        quote.outcome,
        InvokeOutcome::Success(ScVal::U128(_))
    ));
    assert_eq!(quote.auth_mode, AppliedAuthMode::Enforce);
    assert_eq!(quote.disposition, ReceiptDisposition::Previewed);
    assert_eq!(fork.ledger_digest().unwrap(), initial_digest);

    let mint = InvokeRequest {
        contract: address(USDC),
        function: symbol("mint"),
        args: vec![
            ScVal::Address(user.clone()),
            ScVal::I128(Int128Parts {
                hi: 0,
                lo: ONE_USDC,
            }),
        ],
    };
    let mint_preview = fork
        .invoke(mint.clone(), ExecutionMode::Preview, AuthMode::Record)
        .unwrap();
    assert!(mint_preview.outcome.is_success());
    assert_eq!(mint_preview.auth_mode, AppliedAuthMode::RecordMockSatisfied);
    assert_eq!(mint_preview.disposition, ReceiptDisposition::Previewed);
    assert!(!mint_preview.authorization.is_empty());
    assert!(!mint_preview.events.is_empty());
    assert!(!mint_preview.diagnostics.is_empty());
    assert!(!mint_preview.state_changes.is_empty());
    assert_ne!(mint_preview.after_digest, mint_preview.before_digest);
    assert_eq!(fork.ledger_digest().unwrap(), initial_digest);

    let balance_before = fork
        .invoke(
            InvokeRequest {
                contract: address(USDC),
                function: symbol("balance"),
                args: vec![ScVal::Address(user.clone())],
            },
            ExecutionMode::Preview,
            AuthMode::Enforce(vec![]),
        )
        .unwrap();
    assert_eq!(
        balance_before.outcome,
        InvokeOutcome::Success(ScVal::I128(Int128Parts { hi: 0, lo: 0 }))
    );

    let mint_apply = fork
        .invoke(
            mint,
            ExecutionMode::Apply,
            AuthMode::MockExact(mint_preview.authorization.clone()),
        )
        .unwrap();
    assert!(mint_apply.outcome.is_success());
    assert_eq!(mint_apply.auth_mode, AppliedAuthMode::MockExact);
    assert_eq!(mint_apply.disposition, ReceiptDisposition::Committed);
    assert_ne!(fork.ledger_digest().unwrap(), initial_digest);

    let balance_after = fork
        .invoke(
            InvokeRequest {
                contract: address(USDC),
                function: symbol("balance"),
                args: vec![ScVal::Address(user.clone())],
            },
            ExecutionMode::Preview,
            AuthMode::Enforce(vec![]),
        )
        .unwrap();
    assert_eq!(
        balance_after.outcome,
        InvokeOutcome::Success(ScVal::I128(Int128Parts {
            hi: 0,
            lo: ONE_USDC,
        }))
    );

    // The synthetic user's contract instance is RPC-confirmed absent in the
    // capture. Missing-contract failure is therefore evidence, not Unknown.
    let known_absent = fork
        .invoke(
            InvokeRequest {
                contract: user,
                function: symbol("missing"),
                args: vec![],
            },
            ExecutionMode::Apply,
            AuthMode::Enforce(vec![]),
        )
        .unwrap();
    assert!(matches!(
        known_absent.outcome,
        InvokeOutcome::Failure { .. }
    ));
    assert_eq!(known_absent.disposition, ReceiptDisposition::Rejected);
    assert_eq!(known_absent.before_digest, known_absent.after_digest);

    assert_unknown(&mut fork, 0xee);
    assert_ne!(fork.ledger_digest().unwrap(), initial_digest);

    fork.revert(checkpoint).unwrap();
    assert_eq!(fork.ledger_digest().unwrap(), initial_digest);
    assert!(fork.receipts().is_empty());
    assert_unknown(&mut fork, 0xee);
    assert_eq!(fork.upstream_reads(), 0);
}

#[test]
fn mock_exact_mismatch_and_foreign_checkpoint_are_atomic() {
    let captured = CapturedFixture::from_file(CAPTURE, MAINNET_PASSPHRASE).unwrap();
    let mut first = captured.fork();
    let mut second = captured.fork();
    let before = first.ledger_digest().unwrap();
    let user = ScAddress::Contract(ContractId(Hash([0x4b; 32])));
    let mint = InvokeRequest {
        contract: address(USDC),
        function: symbol("mint"),
        args: vec![
            ScVal::Address(user),
            ScVal::I128(Int128Parts { hi: 0, lo: 1 }),
        ],
    };

    let error = first
        .invoke(mint, ExecutionMode::Apply, AuthMode::MockExact(vec![]))
        .unwrap_err();
    assert!(matches!(error, StrictForkError::AuthTreeMismatch));
    assert_eq!(first.ledger_digest().unwrap(), before);

    let checkpoint = first.checkpoint();
    let second_before = second.ledger_digest().unwrap();
    let error = second.revert(checkpoint).unwrap_err();
    assert!(matches!(error, StrictForkError::CheckpointMismatch));
    assert_eq!(second.ledger_digest().unwrap(), second_before);
}

#[test]
fn candidate_production_wasm_hash_constructor_and_local_injection_are_atomic() {
    assert_eq!(
        <[u8; 32]>::from(Sha256::digest(STATEFUL_WASM)),
        STATEFUL_WASM_SHA256
    );
    let captured = CapturedFixture::from_file(CAPTURE, MAINNET_PASSPHRASE).unwrap();
    let mut fork = captured.fork();
    let before = fork.ledger_digest().unwrap();
    let candidate = ScAddress::Contract(ContractId(Hash([0x6b; 32])));

    let error = fork
        .register_candidate(
            candidate.clone(),
            STATEFUL_WASM,
            [0xff; 32],
            vec![ScVal::I64(41)],
        )
        .unwrap_err();
    assert!(matches!(error, StrictForkError::WasmHashMismatch { .. }));
    assert_eq!(fork.ledger_digest().unwrap(), before);

    let registration = fork
        .register_candidate(
            candidate.clone(),
            STATEFUL_WASM,
            STATEFUL_WASM_SHA256,
            vec![ScVal::I64(41)],
        )
        .unwrap();
    assert_eq!(registration.address, candidate);
    assert_eq!(registration.mode, CandidateInstallMode::LocalInjection);
    assert_eq!(registration.wasm_sha256, STATEFUL_WASM_SHA256);
    assert!(!registration.state_changes.is_empty());
    assert_ne!(registration.before_digest, registration.after_digest);
    assert_eq!(registration.upstream_reads, 0);
    assert_ne!(fork.ledger_digest().unwrap(), before);

    let get = || InvokeRequest {
        contract: candidate.clone(),
        function: symbol("get"),
        args: vec![],
    };
    let value = fork
        .invoke(get(), ExecutionMode::Preview, AuthMode::Enforce(vec![]))
        .unwrap();
    assert_eq!(value.outcome, InvokeOutcome::Success(ScVal::I64(41)));

    let checkpoint = fork.checkpoint();
    let increment = fork
        .invoke(
            InvokeRequest {
                contract: candidate.clone(),
                function: symbol("increment"),
                args: vec![ScVal::I64(1)],
            },
            ExecutionMode::Apply,
            AuthMode::Enforce(vec![]),
        )
        .unwrap();
    assert_eq!(increment.outcome, InvokeOutcome::Success(ScVal::I64(42)));
    assert_eq!(increment.disposition, ReceiptDisposition::Committed);
    assert!(!increment.events.is_empty());
    assert!(!increment.state_changes.is_empty());

    fork.revert(checkpoint).unwrap();
    let restored = fork
        .invoke(get(), ExecutionMode::Preview, AuthMode::Enforce(vec![]))
        .unwrap();
    assert_eq!(restored.outcome, InvokeOutcome::Success(ScVal::I64(41)));
    assert_unknown(&mut fork, 0xee);
}

#[test]
fn repeated_mocked_auth_applies_for_one_address_do_not_commit_nonce_scaffolding() {
    let captured = CapturedFixture::from_file(CAPTURE, MAINNET_PASSPHRASE).unwrap();
    let mut fork = captured.fork();
    let candidate = ScAddress::Contract(ContractId(Hash([0x6d; 32])));
    let user = ScAddress::Contract(ContractId(Hash([0x4b; 32])));
    fork.register_candidate(
        candidate.clone(),
        STATEFUL_WASM,
        STATEFUL_WASM_SHA256,
        vec![ScVal::I64(41)],
    )
    .unwrap();
    let checkpoint = fork.checkpoint();
    let increment = || InvokeRequest {
        contract: candidate.clone(),
        function: symbol("authorized_increment"),
        args: vec![ScVal::Address(user.clone()), ScVal::I64(1)],
    };

    let first = fork
        .invoke(increment(), ExecutionMode::Apply, AuthMode::Record)
        .unwrap();
    assert_eq!(first.outcome, InvokeOutcome::Success(ScVal::I64(42)));
    assert!(first.authorization.iter().any(|auth| auth.address == user));
    assert!(!first.state_changes.iter().any(is_nonce_change));
    assert_eq!(fork.ledger_digest().unwrap(), first.after_digest);

    let preview = fork
        .invoke(increment(), ExecutionMode::Preview, AuthMode::Record)
        .unwrap();
    assert_eq!(preview.outcome, InvokeOutcome::Success(ScVal::I64(43)));
    let second = fork
        .invoke(
            increment(),
            ExecutionMode::Apply,
            AuthMode::MockExact(preview.authorization),
        )
        .unwrap();
    assert_eq!(second.outcome, InvokeOutcome::Success(ScVal::I64(43)));
    assert_eq!(second.disposition, ReceiptDisposition::Committed);
    assert!(!second.state_changes.iter().any(is_nonce_change));
    assert_eq!(fork.ledger_digest().unwrap(), second.after_digest);

    fork.revert(checkpoint).unwrap();
    let replayed = fork
        .invoke(increment(), ExecutionMode::Apply, AuthMode::Record)
        .unwrap();
    assert_eq!(replayed.outcome, InvokeOutcome::Success(ScVal::I64(42)));
}

#[test]
fn malformed_enforce_auth_is_typed_and_atomic() {
    let captured = CapturedFixture::from_file(CAPTURE, MAINNET_PASSPHRASE).unwrap();
    let mut fork = captured.fork();
    let candidate = ScAddress::Contract(ContractId(Hash([0x6e; 32])));
    fork.register_candidate(
        candidate.clone(),
        STATEFUL_WASM,
        STATEFUL_WASM_SHA256,
        vec![ScVal::I64(41)],
    )
    .unwrap();
    let request = InvokeRequest {
        contract: candidate.clone(),
        function: symbol("authorized_increment"),
        args: vec![ScVal::Address(candidate.clone()), ScVal::I64(1)],
    };
    let entry = authorization_entry(&request, candidate, ScVal::LedgerKeyContractInstance);
    let before = fork.ledger_digest().unwrap();

    let error = fork
        .invoke(
            request,
            ExecutionMode::Apply,
            AuthMode::Enforce(vec![entry]),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        StrictForkError::InvalidAuthorizationEntries
    ));
    assert_eq!(fork.ledger_digest().unwrap(), before);
}

fn assert_unknown(fork: &mut kanatoko::StrictFork, byte: u8) {
    let before = fork.ledger_digest().unwrap();
    let error = fork
        .invoke(
            InvokeRequest {
                contract: ScAddress::Contract(ContractId(Hash([byte; 32]))),
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

fn is_nonce_change(change: &kanatoko::StateChange) -> bool {
    matches!(
        &change.key,
        LedgerKey::ContractData(data) if matches!(data.key, ScVal::LedgerKeyNonce(_))
    )
}

fn authorization_entry(
    request: &InvokeRequest,
    address: ScAddress,
    signature: ScVal,
) -> SorobanAuthorizationEntry {
    SorobanAuthorizationEntry {
        credentials: SorobanCredentials::AddressV2(SorobanAddressCredentials {
            address,
            nonce: 7,
            signature_expiration_ledger: 63_600_396,
            signature,
        }),
        root_invocation: SorobanAuthorizedInvocation {
            function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                contract_address: request.contract.clone(),
                function_name: request.function.clone(),
                args: request.args.clone().try_into().unwrap(),
            }),
            sub_invocations: Vec::new().try_into().unwrap(),
        },
    }
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
    ScAddress::from(&Address::from_str(&env, value))
}

fn symbol(value: &str) -> ScSymbol {
    ScSymbol(value.try_into().unwrap())
}
