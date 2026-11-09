//! Runnable CLI for Soroswap capture and strict-fork workflows.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use kanatoko::{
    AuthMode, CandidateRegistration, CaptureBuilder, CaptureProvenance, CapturedFixture,
    ExecutionMode, InvokeErrorKind, InvokeOutcome, InvokeRequest, LedgerValue, Receipt,
    StateChange, StrictFork, StrictForkError,
};
use serde_json::{json, Value};
use soroban_env_host::xdr::{
    ContractId, Hash, Int128Parts, Limits, ScAddress, ScSymbol, ScVal, WriteXdr,
};
use soroban_sdk::{
    testutils::{
        AuthorizedFunction, AuthorizedInvocation, EnvTestConfig, MockAuth, MockAuthInvoke,
    },
    token::{StellarAssetClient, TokenClient},
    Address, Env, IntoVal, Symbol, TryFromVal,
};

mod soroswap {
    #![allow(clippy::too_many_arguments)]

    soroban_sdk::contractimport!(
        file = "fixtures/wasm/kanatoko_soroswap_wrapper.wasm",
        sha256 = "1e7bca07c5aa5c2f31544857027799285dff97266ed7895cf21d0e6736dcb3a8",
    );
}

const DEFAULT_RPC_URL: &str = "https://mainnet.sorobanrpc.com";
const DEFAULT_ROUTER: &str = "CAG5LRYQ5JVEUI5TEID72EYOVX44TTUJT5BQR2J6J77FH65PCCFAJDDH";
const DEFAULT_PAIR: &str = "CAM7DY53G63XA4AJRS24Z6VFYAFSSF76C3RZ45BE5YU3FQS5255OOABP";
const DEFAULT_BUNDLE: &str = "fixtures/mainnet/soroswap-xlm-usdc/capture.json";
const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const WRAPPER_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_soroswap_wrapper.wasm");
const WRAPPER_SHA256: [u8; 32] = [
    0x1e, 0x7b, 0xca, 0x07, 0xc5, 0xaa, 0x5c, 0x2f, 0x31, 0x54, 0x48, 0x57, 0x02, 0x77, 0x99, 0x28,
    0x5d, 0xff, 0x97, 0x26, 0x6e, 0xd7, 0x89, 0x5c, 0xf2, 0x1d, 0x0e, 0x67, 0x36, 0xdc, 0xb3, 0xa8,
];
const ONE_TOKEN: i128 = 10_000_000;
const DEADLINE: u64 = u64::MAX;

fn main() -> Result<(), Box<dyn Error>> {
    match Options::parse()? {
        Options::Help => {
            print_help();
            Ok(())
        }
        Options::Capture {
            rpc_url,
            bundle,
            allow_newer_protocol,
        } => capture(&rpc_url, &bundle, allow_newer_protocol),
        Options::Run {
            fixture,
            candidate,
            format,
            allow_newer_protocol,
        } => run(&fixture, &candidate, format, allow_newer_protocol),
    }
}

fn capture(
    rpc_url: &str,
    bundle_path: &Path,
    allow_newer_protocol: bool,
) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = bundle_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let mut builder = CaptureBuilder::mainnet(rpc_url)?;
    if allow_newer_protocol {
        builder = builder.allow_newer_protocol();
    }
    let captured = builder.capture(soroswap_scenario)?;
    warn_if_newer_protocol(captured.provenance());
    captured.write_file(bundle_path)?;

    // Read back through the public fail-closed loader and prove that the exact
    // saved artifact, rather than the in-memory capture, replays offline.
    let loaded = load(bundle_path, allow_newer_protocol)?;
    loaded.replay(soroswap_scenario)?;

    println!("captured ledger {}", loaded.provenance().ledger_sequence());
    println!("source origin: {}", loaded.provenance().rpc_origin());
    println!("scenario router: {DEFAULT_ROUTER}");
    println!("scenario pair: {DEFAULT_PAIR}");
    println!("present entries: {}", loaded.report().present_entries());
    println!(
        "confirmed absent entries: {}",
        loaded.report().absent_entries()
    );
    println!("bundle: {}", bundle_path.display());
    println!("offline replay: ok");
    Ok(())
}

fn load(bundle_path: &Path, allow_newer_protocol: bool) -> Result<CapturedFixture, Box<dyn Error>> {
    Ok(if allow_newer_protocol {
        CapturedFixture::from_file_allowing_newer_protocol(bundle_path, MAINNET_PASSPHRASE)?
    } else {
        CapturedFixture::from_file(bundle_path, MAINNET_PASSPHRASE)?
    })
}

/// Prints one stderr line when a newer-protocol ledger runs on the older Host.
fn warn_if_newer_protocol(provenance: &CaptureProvenance) {
    if provenance.protocol_version() != provenance.executed_protocol_version() {
        eprintln!(
            "warning: --allow-newer-protocol: executing Protocol {} ledger {} on the Protocol {} Host; Host behaviour may differ from the network",
            provenance.protocol_version(),
            provenance.ledger_sequence(),
            provenance.executed_protocol_version(),
        );
    }
}

fn soroswap_scenario(env: &Env) {
    let router_id = Address::from_str(env, DEFAULT_ROUTER);
    let pair_id = Address::from_str(env, DEFAULT_PAIR);
    let router = soroswap::Client::new(env, &router_id);
    let pair = soroswap::Client::new(env, &pair_id);
    let xlm = pair.token_0();
    let usdc = pair.token_1();
    let usdc_token = TokenClient::new(env, &usdc);
    let usdc_admin = StellarAssetClient::new(env, &usdc);
    let one_usdc = 10_i128
        .checked_pow(usdc_token.decimals())
        .expect("token decimals must fit i128");
    let path = soroban_sdk::vec![env, usdc.clone(), xlm];

    let quote_before = router
        .router_get_amounts_out(&one_usdc, &path)
        .get(1)
        .expect("router quote must have an output amount");
    assert!(quote_before > 0);
    let (_, usdc_reserve) = pair.get_reserves();
    let minted = usdc_reserve / 10;
    assert!(minted > one_usdc);
    let user = deterministic_contract_user(env);

    // The live SAC admin can itself be a contract. Recording mode observes the
    // real admin authorization without replacing imported contract state.
    let admin = usdc_admin.admin();
    env.mock_all_auths();
    usdc_admin.mint(&user, &minted);
    assert_eq!(
        env.auths(),
        [(
            admin,
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    usdc.clone(),
                    Symbol::new(env, "mint"),
                    (user.clone(), minted).into_val(env),
                )),
                sub_invocations: std::vec![],
            },
        )]
    );
    assert_eq!(usdc_token.balance(&user), minted);
    env.set_auths(&[]);

    let transfer = MockAuthInvoke {
        contract: &usdc,
        fn_name: "transfer",
        args: (user.clone(), pair_id.clone(), minted).into_val(env),
        sub_invokes: &[],
    };
    let nested = [transfer];
    let swap_args = (minted, 0_i128, path.clone(), user.clone(), DEADLINE);
    let swap = MockAuthInvoke {
        contract: &router_id,
        fn_name: "swap_exact_tokens_for_tokens",
        args: swap_args.clone().into_val(env),
        sub_invokes: &nested,
    };
    let amounts = router
        .mock_auths(&[MockAuth {
            address: &user,
            invoke: &swap,
        }])
        .swap_exact_tokens_for_tokens(&minted, &0, &path, &user, &DEADLINE);
    assert!(amounts.get(1).expect("swap must have an output amount") > 0);
    assert_eq!(
        env.auths(),
        [(
            user.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    router_id.clone(),
                    Symbol::new(env, "swap_exact_tokens_for_tokens"),
                    swap_args.into_val(env),
                )),
                sub_invocations: std::vec![AuthorizedInvocation {
                    function: AuthorizedFunction::Contract((
                        usdc,
                        Symbol::new(env, "transfer"),
                        (user, pair_id, minted).into_val(env),
                    )),
                    sub_invocations: std::vec![],
                }],
            },
        )]
    );

    let quote_after = router
        .router_get_amounts_out(&one_usdc, &path)
        .get(1)
        .expect("router quote must have an output amount");
    assert_ne!(quote_after, quote_before);
    assert!(quote_after < quote_before);
}

fn deterministic_contract_user(env: &Env) -> Address {
    Address::try_from_val(env, &ScAddress::Contract(Hash([0x4b; 32]).into()))
        .expect("synthetic contract user must be a valid Address")
}

#[allow(clippy::too_many_lines)]
fn run(
    fixture_path: &PathBuf,
    candidate_artifact: &CandidateArtifact,
    format: OutputFormat,
    allow_newer_protocol: bool,
) -> Result<(), Box<dyn Error>> {
    let captured = if allow_newer_protocol {
        CapturedFixture::from_file_allowing_newer_protocol(fixture_path, MAINNET_PASSPHRASE)?
    } else {
        CapturedFixture::from_file(fixture_path, MAINNET_PASSPHRASE)?
    };
    warn_if_newer_protocol(captured.provenance());
    let mut fork = captured.fork();
    let router = address(DEFAULT_ROUTER);
    let pair = address(DEFAULT_PAIR);
    let candidate = ScAddress::Contract(ContractId(Hash([0x6c; 32])));
    let user = ScAddress::Contract(ContractId(Hash([0x4b; 32])));
    let wasm = candidate_artifact.bytes()?;
    let registration = fork.register_candidate(
        candidate.clone(),
        &wasm,
        candidate_artifact.sha256(),
        vec![ScVal::Address(router), ScVal::Address(pair)],
    )?;
    let checkpoint = fork.checkpoint();

    let mut receipts = Vec::new();
    let xlm_receipt = invoke_success(
        &mut fork,
        call(&candidate, "token_0", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    )?;
    let ScVal::Address(xlm) = result(&xlm_receipt)?.clone() else {
        return Err("candidate token_0 did not return an address".into());
    };
    receipts.push(xlm_receipt);
    let usdc_receipt = invoke_success(
        &mut fork,
        call(&candidate, "token_1", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    )?;
    let ScVal::Address(usdc) = result(&usdc_receipt)?.clone() else {
        return Err("candidate token_1 did not return an address".into());
    };
    receipts.push(usdc_receipt);
    let path = sc_path(&usdc, &xlm)?;

    let quote_before_receipt = quote_receipt(&mut fork, &candidate, &path)?;
    let quote_before = quoted_output(result(&quote_before_receipt)?)?;
    receipts.push(quote_before_receipt);
    let reserves_receipt = invoke_success(
        &mut fork,
        call(&candidate, "get_reserves", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    )?;
    let reserves = sc_i128_vec(result(&reserves_receipt)?)?;
    let minted = *reserves
        .get(1)
        .ok_or("candidate get_reserves did not return two reserves")?
        / 10;
    if minted <= ONE_TOKEN {
        return Err("10% of captured USDC reserve is not greater than one token".into());
    }
    receipts.push(reserves_receipt);

    let mint = invoke_success(
        &mut fork,
        InvokeRequest {
            contract: usdc,
            function: symbol("mint"),
            args: vec![ScVal::Address(user.clone()), sc_i128(minted)],
        },
        ExecutionMode::Apply,
        AuthMode::Record,
    )?;
    receipts.push(mint);

    let swap_request = call(
        &candidate,
        "swap_exact_tokens_for_tokens",
        vec![
            sc_i128(minted),
            sc_i128(0),
            path.clone(),
            ScVal::Address(user),
            ScVal::U64(DEADLINE),
        ],
    );
    let preview = invoke_success(
        &mut fork,
        swap_request.clone(),
        ExecutionMode::Preview,
        AuthMode::Record,
    )?;
    let exact_auth = preview.authorization.clone();
    receipts.push(preview);
    let swap = invoke_success(
        &mut fork,
        swap_request,
        ExecutionMode::Apply,
        AuthMode::MockExact(exact_auth),
    )?;
    let received = quoted_output(result(&swap)?)?;
    receipts.push(swap);

    let quote_after_receipt = quote_receipt(&mut fork, &candidate, &path)?;
    let quote_after = quoted_output(result(&quote_after_receipt)?)?;
    receipts.push(quote_after_receipt);
    if quote_after >= quote_before {
        return Err("post-swap 1 USDC -> XLM quote did not decrease".into());
    }

    fork.revert(checkpoint)?;
    let restored_receipt = quote_receipt(&mut fork, &candidate, &path)?;
    let restored_quote = quoted_output(result(&restored_receipt)?)?;
    receipts.push(restored_receipt);
    if restored_quote != quote_before {
        return Err("checkpoint revert did not restore the initial quote".into());
    }
    let unknown_fail_closed = unknown_fails_closed(&mut fork)?;
    if fork.upstream_reads() != 0 || receipts.iter().any(|receipt| receipt.upstream_reads != 0) {
        return Err("strict offline run reported an upstream read".into());
    }

    // Protocol traceability is added only in the opt-in mode, so default
    // output stays identical to earlier releases.
    let protocols = allow_newer_protocol.then(|| Protocols::of(&fork));
    let mut report = json!({
        "evidence": ["contract-functional", "state-reproducible"],
        "transactionFaithfulDeploy": false,
        "fixture": fixture_path,
        "ledger": captured.provenance().ledger_sequence(),
        "scenarioRouter": DEFAULT_ROUTER,
        "scenarioPair": DEFAULT_PAIR,
        "candidateRegistration": registration_json(&registration),
        "mintedUsdc": minted.to_string(),
        "receivedXlm": received.to_string(),
        "quoteBefore": quote_before.to_string(),
        "quoteAfter": quote_after.to_string(),
        "restoredQuote": restored_quote.to_string(),
        "unknownFailClosed": unknown_fail_closed,
        "upstreamReads": fork.upstream_reads(),
        "receipts": receipts
            .iter()
            .map(|receipt| receipt_json(receipt, protocols))
            .collect::<Vec<_>>(),
    });
    if let Some(protocols) = protocols {
        protocols.insert_into(&mut report);
    }
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Text => {
            println!("strict Soroswap run: ok");
            println!("ledger: {}", captured.provenance().ledger_sequence());
            if let Some(protocols) = protocols {
                println!("network protocol: {}", protocols.network);
                println!("executed protocol: {}", protocols.executed);
            }
            println!("candidate install: local injection (not transaction-faithful deploy)");
            println!("quote before: {quote_before}");
            println!("quote after: {quote_after}");
            println!("revert restored quote: {restored_quote}");
            println!("unknown key fail-closed: {unknown_fail_closed}");
            println!("upstream reads: {}", fork.upstream_reads());
            println!(
                "receipts: {} (use --format json for detached XDR)",
                receipts.len()
            );
        }
    }
    Ok(())
}

fn quote_receipt(
    fork: &mut StrictFork,
    candidate: &ScAddress,
    path: &ScVal,
) -> Result<Receipt, Box<dyn Error>> {
    invoke_success(
        fork,
        call(
            candidate,
            "router_get_amounts_out",
            vec![sc_i128(ONE_TOKEN), path.clone()],
        ),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    )
}

fn invoke_success(
    fork: &mut StrictFork,
    request: InvokeRequest,
    execution: ExecutionMode,
    auth: AuthMode,
) -> Result<Receipt, Box<dyn Error>> {
    let receipt = fork.invoke(request, execution, auth)?;
    if !receipt.outcome.is_success() {
        return Err(format!("contract invocation failed: {:?}", receipt.outcome).into());
    }
    Ok(receipt)
}

fn unknown_fails_closed(fork: &mut StrictFork) -> Result<bool, Box<dyn Error>> {
    let before = fork.ledger_digest()?;
    let outcome = fork.invoke(
        InvokeRequest {
            contract: ScAddress::Contract(ContractId(Hash([0xee; 32]))),
            function: symbol("missing"),
            args: vec![],
        },
        ExecutionMode::Apply,
        AuthMode::Enforce(vec![]),
    );
    let failed = matches!(outcome, Err(StrictForkError::UnknownLedgerKeys { .. }));
    Ok(failed && fork.ledger_digest()? == before)
}

fn result(receipt: &Receipt) -> Result<&ScVal, Box<dyn Error>> {
    match &receipt.outcome {
        InvokeOutcome::Success(value) => Ok(value),
        InvokeOutcome::Failure { .. } => Err("receipt has no successful result".into()),
    }
}

fn registration_json(registration: &CandidateRegistration) -> Value {
    json!({
        "addressXdr": xdr_b64(&registration.address),
        "wasmSha256": hex(registration.wasm_sha256),
        "mode": "local_injection",
        "transactionFaithful": false,
        "beforeDigest": hex(registration.before_digest),
        "afterDigest": hex(registration.after_digest),
        "upstreamReads": registration.upstream_reads,
        "stateDiff": registration.state_changes.iter().map(state_change_json).collect::<Vec<_>>(),
    })
}

/// Network and executed ledger protocols of a strict fork.
#[derive(Clone, Copy)]
struct Protocols {
    network: u32,
    executed: u32,
}

impl Protocols {
    const fn of(fork: &StrictFork) -> Self {
        Self {
            network: fork.network_protocol_version(),
            executed: fork.executed_protocol_version(),
        }
    }

    fn insert_into(self, value: &mut Value) {
        if let Some(object) = value.as_object_mut() {
            object.insert("networkProtocol".to_string(), json!(self.network));
            object.insert("executedProtocol".to_string(), json!(self.executed));
        }
    }
}

fn receipt_json(receipt: &Receipt, protocols: Option<Protocols>) -> Value {
    let outcome = match &receipt.outcome {
        InvokeOutcome::Success(value) => json!({"status": "success", "resultXdr": xdr_b64(value)}),
        InvokeOutcome::Failure { error, kind } => json!({
            "status": "error",
            "errorXdr": error.as_ref().map(xdr_b64),
            "kind": invoke_error_name(*kind),
        }),
    };
    let mut value = json!({
        "request": {
            "contractXdr": xdr_b64(&receipt.request.contract),
            "functionXdr": xdr_b64(&receipt.request.function),
            "argsXdr": receipt.request.args.iter().map(xdr_b64).collect::<Vec<_>>(),
        },
        "outcome": outcome,
        "authMode": format!("{:?}", receipt.auth_mode),
        "auth": receipt.authorization.iter().map(|auth| json!({
            "addressXdr": xdr_b64(&auth.address),
            "invocationXdr": xdr_b64(&auth.invocation),
        })).collect::<Vec<_>>(),
        "events": receipt.events.iter().map(|event| json!({
            "eventXdr": xdr_b64(&event.event),
            "failedCall": event.failed_call,
        })).collect::<Vec<_>>(),
        "diagnostics": receipt.diagnostics.iter().map(|event| json!({
            "eventXdr": xdr_b64(&event.event),
            "failedCall": event.failed_call,
        })).collect::<Vec<_>>(),
        "stateDiff": receipt.state_changes.iter().map(state_change_json).collect::<Vec<_>>(),
        "beforeDigest": hex(receipt.before_digest),
        "afterDigest": hex(receipt.after_digest),
        "disposition": format!("{:?}", receipt.disposition),
        "upstreamReads": receipt.upstream_reads,
    });
    if let Some(protocols) = protocols {
        protocols.insert_into(&mut value);
    }
    value
}

fn state_change_json(change: &StateChange) -> Value {
    json!({
        "keyXdr": xdr_b64(&change.key),
        "before": change.before.as_ref().map(ledger_value_json),
        "after": change.after.as_ref().map(ledger_value_json),
    })
}

fn ledger_value_json(value: &LedgerValue) -> Value {
    json!({
        "entryXdr": xdr_b64(&value.entry),
        "liveUntilLedger": value.live_until_ledger,
    })
}

fn invoke_error_name(kind: InvokeErrorKind) -> String {
    format!("{kind:?}")
}

fn xdr_b64(value: &impl WriteXdr) -> String {
    BASE64.encode(
        value
            .to_xdr(Limits::none())
            .expect("Host-produced detached XDR must encode"),
    )
}

fn hex(bytes: [u8; 32]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn sc_vec(value: &ScVal) -> Vec<ScVal> {
    match value {
        ScVal::Vec(Some(values)) => values.0.to_vec(),
        _ => Vec::new(),
    }
}

fn sc_i128_vec(value: &ScVal) -> Result<Vec<i128>, Box<dyn Error>> {
    sc_vec(value).iter().map(sc_i128_value).collect()
}

fn sc_i128_value(value: &ScVal) -> Result<i128, Box<dyn Error>> {
    let ScVal::I128(parts) = value else {
        return Err("result is not i128".into());
    };
    Ok((i128::from(parts.hi) << 64) | i128::from(parts.lo))
}

fn quoted_output(value: &ScVal) -> Result<i128, Box<dyn Error>> {
    sc_i128_vec(value)?
        .get(1)
        .copied()
        .ok_or_else(|| "result has no output amount".into())
}

fn sc_path(input: &ScAddress, output: &ScAddress) -> Result<ScVal, Box<dyn Error>> {
    let path = vec![
        ScVal::Address(input.clone()),
        ScVal::Address(output.clone()),
    ];
    Ok(ScVal::Vec(Some(path.try_into()?)))
}

fn sc_i128(value: i128) -> ScVal {
    let bytes = value.to_be_bytes();
    ScVal::I128(Int128Parts {
        hi: i64::from_be_bytes(bytes[..8].try_into().expect("eight-byte high half")),
        lo: u64::from_be_bytes(bytes[8..].try_into().expect("eight-byte low half")),
    })
}

fn call(contract: &ScAddress, function: &str, args: Vec<ScVal>) -> InvokeRequest {
    InvokeRequest {
        contract: contract.clone(),
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
    ScSymbol(value.try_into().expect("valid Soroban symbol"))
}

enum Options {
    Help,
    Capture {
        rpc_url: String,
        bundle: PathBuf,
        allow_newer_protocol: bool,
    },
    Run {
        fixture: PathBuf,
        candidate: CandidateArtifact,
        format: OutputFormat,
        allow_newer_protocol: bool,
    },
}

impl Options {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut args = std::env::args().skip(1);
        let Some(command) = args.next() else {
            return Ok(Self::Help);
        };
        if matches!(command.as_str(), "help" | "--help" | "-h") {
            return Ok(Self::Help);
        }
        let scenario = args
            .next()
            .ok_or("expected scenario name soroswap-xlm-usdc")?;
        if scenario != "soroswap-xlm-usdc" {
            return Err(format!("unsupported scenario: {scenario}").into());
        }
        match command.as_str() {
            "capture" => {
                let mut rpc_url = DEFAULT_RPC_URL.to_string();
                let mut bundle = PathBuf::from(DEFAULT_BUNDLE);
                let mut allow_newer_protocol = false;
                while let Some(option) = args.next() {
                    match option.as_str() {
                        "--rpc-url" => rpc_url = next(&mut args, &option)?,
                        "--bundle" => bundle = PathBuf::from(next(&mut args, &option)?),
                        "--allow-newer-protocol" => allow_newer_protocol = true,
                        _ => return Err(format!("unknown capture option: {option}").into()),
                    }
                }
                Ok(Self::Capture {
                    rpc_url,
                    bundle,
                    allow_newer_protocol,
                })
            }
            "run" => {
                let mut fixture = PathBuf::from(DEFAULT_BUNDLE);
                let mut candidate_path = None;
                let mut candidate_hash = None;
                let mut format = OutputFormat::Text;
                let mut allow_newer_protocol = false;
                while let Some(option) = args.next() {
                    match option.as_str() {
                        "--allow-newer-protocol" => allow_newer_protocol = true,
                        "--fixture" | "--bundle" => {
                            fixture = PathBuf::from(next(&mut args, &option)?);
                        }
                        "--candidate-wasm" => {
                            candidate_path = Some(PathBuf::from(next(&mut args, &option)?));
                        }
                        "--candidate-sha256" => {
                            candidate_hash = Some(parse_hash(&next(&mut args, &option)?)?);
                        }
                        "--format" => {
                            format = match next(&mut args, &option)?.as_str() {
                                "json" => OutputFormat::Json,
                                "text" => OutputFormat::Text,
                                value => return Err(format!("unsupported format: {value}").into()),
                            };
                        }
                        _ => return Err(format!("unknown run option: {option}").into()),
                    }
                }
                let candidate = match (candidate_path, candidate_hash) {
                    (None, None) => CandidateArtifact::Bundled,
                    (Some(path), Some(sha256)) => CandidateArtifact::File { path, sha256 },
                    _ => return Err(
                        "custom candidate requires both --candidate-wasm and --candidate-sha256"
                            .into(),
                    ),
                };
                Ok(Self::Run {
                    fixture,
                    candidate,
                    format,
                    allow_newer_protocol,
                })
            }
            _ => Err(format!("unknown command: {command}").into()),
        }
    }
}

enum CandidateArtifact {
    Bundled,
    File { path: PathBuf, sha256: [u8; 32] },
}

impl CandidateArtifact {
    fn bytes(&self) -> Result<Vec<u8>, Box<dyn Error>> {
        Ok(match self {
            Self::Bundled => WRAPPER_WASM.to_vec(),
            Self::File { path, .. } => fs::read(path)?,
        })
    }

    const fn sha256(&self) -> [u8; 32] {
        match self {
            Self::Bundled => WRAPPER_SHA256,
            Self::File { sha256, .. } => *sha256,
        }
    }
}

#[derive(Clone, Copy)]
enum OutputFormat {
    Text,
    Json,
}

fn next(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("{option} needs a value").into())
}

fn parse_hash(value: &str) -> Result<[u8; 32], Box<dyn Error>> {
    if value.len() != 64 || !value.is_ascii() {
        return Err("SHA-256 must be 64 lowercase hex characters".into());
    }
    let mut hash = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        hash[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    if hex(hash) != value {
        return Err("SHA-256 must be canonical lowercase hex".into());
    }
    Ok(hash)
}

fn hex_nibble(value: u8) -> Result<u8, Box<dyn Error>> {
    Ok(match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => return Err("invalid lowercase hex".into()),
    })
}

fn print_help() {
    println!(
        "Kanatoko strict Soroban fork workflow\n\n\
         Capture the Soroswap XLM/USDC scenario:\n  \
         kanatoko capture soroswap-xlm-usdc [--rpc-url URL] [--bundle PATH]\n  \
         [--allow-newer-protocol]\n\n\
         Run the strict workflow fully offline:\n  \
         kanatoko run soroswap-xlm-usdc [--fixture PATH] [--format text|json]\n  \
         [--candidate-wasm PATH --candidate-sha256 HEX] [--allow-newer-protocol]\n\n\
         --allow-newer-protocol accepts a ledger exactly one protocol newer than\n\
         the Host and executes it on the Host's own protocol. Captures keep the\n\
         real network protocol; Host behaviour may differ from the network.\n"
    );
}
