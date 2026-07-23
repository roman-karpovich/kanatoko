//! Runnable CLI for Aquarius capture and strict-fork workflows.

use std::{error::Error, fs, path::PathBuf};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use kanatoko::{
    AuthMode, CandidateRegistration, CapturedFixture, ExecutionMode, InvokeErrorKind,
    InvokeOutcome, InvokeRequest, LedgerValue, Receipt, StateChange, StrictFork, StrictForkError,
};
use serde_json::{json, Value};
use soroban_env_host::xdr::{
    ContractId, Hash, Int128Parts, Limits, ScAddress, ScSymbol, ScVal, UInt128Parts, WriteXdr,
};
use soroban_sdk::{testutils::EnvTestConfig, Address, Env};

#[allow(dead_code)]
#[path = "capture_aquarius_xlm_usdc_cp.rs"]
mod aquarius_capture;

const DEFAULT_RPC_URL: &str = "https://mainnet.sorobanrpc.com";
const DEFAULT_POOL: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
const DEFAULT_BUNDLE: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/capture.json";
const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const WRAPPER_WASM: &[u8] = include_bytes!("../fixtures/wasm/kanatoko_aquarius_wrapper.wasm");
const WRAPPER_SHA256: [u8; 32] = [
    0x79, 0x8c, 0x95, 0x9e, 0x1e, 0x22, 0x09, 0x3c, 0x49, 0xb4, 0xec, 0x66, 0x36, 0xaa, 0xfe, 0xd1,
    0x4e, 0x88, 0x96, 0x14, 0xfb, 0x24, 0x34, 0x26, 0xab, 0xe5, 0x02, 0x3b, 0x30, 0xc1, 0x75, 0x20,
];
const ONE_TOKEN: u128 = 10_000_000;

fn main() -> Result<(), Box<dyn Error>> {
    match Options::parse()? {
        Options::Help => {
            print_help();
            Ok(())
        }
        Options::Capture { rpc_url, bundle } => aquarius_capture::capture(&rpc_url, &bundle),
        Options::Run {
            fixture,
            candidate,
            format,
        } => run(&fixture, &candidate, format),
    }
}

#[allow(clippy::too_many_lines)]
fn run(
    fixture_path: &PathBuf,
    candidate_artifact: &CandidateArtifact,
    format: OutputFormat,
) -> Result<(), Box<dyn Error>> {
    let captured = CapturedFixture::from_file(fixture_path, MAINNET_PASSPHRASE)?;
    let mut fork = captured.fork();
    let pool = address(DEFAULT_POOL);
    let candidate = ScAddress::Contract(ContractId(Hash([0x6c; 32])));
    let user = ScAddress::Contract(ContractId(Hash([0x4b; 32])));
    let wasm = candidate_artifact.bytes()?;
    let registration = fork.register_candidate(
        candidate.clone(),
        &wasm,
        candidate_artifact.sha256(),
        vec![ScVal::Address(pool)],
    )?;
    let checkpoint = fork.checkpoint();

    let mut receipts = Vec::new();
    let tokens_receipt = invoke_success(
        &mut fork,
        call(&candidate, "get_tokens", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    )?;
    let tokens = sc_vec(result(&tokens_receipt)?);
    let usdc = match tokens.get(1) {
        Some(ScVal::Address(address)) => address.clone(),
        _ => return Err("candidate get_tokens did not return a second address".into()),
    };
    receipts.push(tokens_receipt);

    let quote_before_receipt = quote_receipt(&mut fork, &candidate)?;
    let quote_before = sc_u128_value(result(&quote_before_receipt)?)?;
    receipts.push(quote_before_receipt);
    let reserves_receipt = invoke_success(
        &mut fork,
        call(&candidate, "get_reserves", vec![]),
        ExecutionMode::Preview,
        AuthMode::Enforce(vec![]),
    )?;
    let reserves = sc_u128_vec(result(&reserves_receipt)?)?;
    let minted = *reserves
        .get(1)
        .ok_or("candidate get_reserves did not return two reserves")?
        / 10;
    if minted <= ONE_TOKEN {
        return Err("10% of captured USDC reserve is not greater than one token".into());
    }
    let minted_i128 = i128::try_from(minted)?;
    receipts.push(reserves_receipt);

    let mint = invoke_success(
        &mut fork,
        InvokeRequest {
            contract: usdc,
            function: symbol("mint"),
            args: vec![ScVal::Address(user.clone()), sc_i128(minted_i128)],
        },
        ExecutionMode::Apply,
        AuthMode::Record,
    )?;
    receipts.push(mint);

    let swap_request = call(
        &candidate,
        "swap",
        vec![
            ScVal::Address(user),
            ScVal::U32(1),
            ScVal::U32(0),
            sc_u128(minted),
            sc_u128(0),
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
    let received = sc_u128_value(result(&swap)?)?;
    receipts.push(swap);

    let quote_after_receipt = quote_receipt(&mut fork, &candidate)?;
    let quote_after = sc_u128_value(result(&quote_after_receipt)?)?;
    receipts.push(quote_after_receipt);
    if quote_after >= quote_before {
        return Err("post-swap 1 USDC -> XLM quote did not decrease".into());
    }

    fork.revert(checkpoint)?;
    let restored_receipt = quote_receipt(&mut fork, &candidate)?;
    let restored_quote = sc_u128_value(result(&restored_receipt)?)?;
    receipts.push(restored_receipt);
    if restored_quote != quote_before {
        return Err("checkpoint revert did not restore the initial quote".into());
    }
    let unknown_fail_closed = unknown_fails_closed(&mut fork)?;
    if fork.upstream_reads() != 0 || receipts.iter().any(|receipt| receipt.upstream_reads != 0) {
        return Err("strict offline run reported an upstream read".into());
    }

    let report = json!({
        "evidence": ["contract-functional", "state-reproducible"],
        "transactionFaithfulDeploy": false,
        "fixture": fixture_path,
        "ledger": captured.provenance().ledger_sequence(),
        "scenarioPool": DEFAULT_POOL,
        "candidateRegistration": registration_json(&registration),
        "mintedUsdc": minted.to_string(),
        "receivedXlm": received.to_string(),
        "quoteBefore": quote_before.to_string(),
        "quoteAfter": quote_after.to_string(),
        "restoredQuote": restored_quote.to_string(),
        "unknownFailClosed": unknown_fail_closed,
        "upstreamReads": fork.upstream_reads(),
        "receipts": receipts.iter().map(receipt_json).collect::<Vec<_>>(),
    });
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Text => {
            println!("strict Aquarius run: ok");
            println!("ledger: {}", captured.provenance().ledger_sequence());
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

fn quote_receipt(fork: &mut StrictFork, candidate: &ScAddress) -> Result<Receipt, Box<dyn Error>> {
    invoke_success(
        fork,
        call(
            candidate,
            "estimate_swap",
            vec![ScVal::U32(1), ScVal::U32(0), sc_u128(ONE_TOKEN)],
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

fn receipt_json(receipt: &Receipt) -> Value {
    let outcome = match &receipt.outcome {
        InvokeOutcome::Success(value) => json!({"status": "success", "resultXdr": xdr_b64(value)}),
        InvokeOutcome::Failure { error, kind } => json!({
            "status": "error",
            "errorXdr": error.as_ref().map(xdr_b64),
            "kind": invoke_error_name(*kind),
        }),
    };
    json!({
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
    })
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

fn sc_u128_vec(value: &ScVal) -> Result<Vec<u128>, Box<dyn Error>> {
    sc_vec(value).iter().map(sc_u128_value).collect()
}

fn sc_u128_value(value: &ScVal) -> Result<u128, Box<dyn Error>> {
    let ScVal::U128(parts) = value else {
        return Err("result is not u128".into());
    };
    Ok((u128::from(parts.hi) << 64) | u128::from(parts.lo))
}

fn sc_u128(value: u128) -> ScVal {
    let bytes = value.to_be_bytes();
    ScVal::U128(UInt128Parts {
        hi: u64::from_be_bytes(bytes[..8].try_into().expect("eight-byte high half")),
        lo: u64::from_be_bytes(bytes[8..].try_into().expect("eight-byte low half")),
    })
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
    },
    Run {
        fixture: PathBuf,
        candidate: CandidateArtifact,
        format: OutputFormat,
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
        let scenario = args.next().ok_or("expected scenario name aquarius-cp")?;
        if scenario != "aquarius-cp" {
            return Err(format!("unsupported scenario: {scenario}").into());
        }
        match command.as_str() {
            "capture" => {
                let mut rpc_url = DEFAULT_RPC_URL.to_string();
                let mut bundle = PathBuf::from(DEFAULT_BUNDLE);
                while let Some(option) = args.next() {
                    match option.as_str() {
                        "--rpc-url" => rpc_url = next(&mut args, &option)?,
                        "--bundle" => bundle = PathBuf::from(next(&mut args, &option)?),
                        _ => return Err(format!("unknown capture option: {option}").into()),
                    }
                }
                Ok(Self::Capture { rpc_url, bundle })
            }
            "run" => {
                let mut fixture = PathBuf::from(DEFAULT_BUNDLE);
                let mut candidate_path = None;
                let mut candidate_hash = None;
                let mut format = OutputFormat::Text;
                while let Some(option) = args.next() {
                    match option.as_str() {
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
         Capture the Aquarius scenario:\n  \
         kanatoko capture aquarius-cp [--rpc-url URL] [--bundle PATH]\n\n\
         Run the strict workflow fully offline:\n  \
         kanatoko run aquarius-cp [--fixture PATH] [--format text|json]\n  \
         [--candidate-wasm PATH --candidate-sha256 HEX]\n"
    );
}
