#![cfg(all(feature = "capture", kanatoko_protocol_28_fixtures))]

use std::process::{Command, Output};

use serde_json::Value;

const CAPTURE_P29: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/capture-p29.json";

#[test]
fn cli_runs_the_strict_aquarius_workflow_offline() {
    let output = run_offline(&["run", "aquarius-cp", "--format", "text"]);

    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("strict Aquarius run: ok"));
    assert!(stdout.contains("unknown key fail-closed: true"));
    assert!(stdout.contains("upstream reads: 0"));
    assert!(stdout.contains("not transaction-faithful deploy"));
    // Without the opt-in the output carries no protocol traceability lines.
    assert!(!stdout.contains("protocol"));
    assert!(output.stderr.is_empty());
}

#[test]
fn cli_rejects_a_protocol_29_fixture_without_the_opt_in() {
    let output = run_offline(&["run", "aquarius-cp", "--fixture", CAPTURE_P29]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("UnsupportedProtocol { found: 29, supported: 28 }"),
        "{stderr}"
    );
}

#[test]
fn cli_runs_a_protocol_29_fixture_offline_with_the_opt_in() {
    let output = run_offline(&[
        "run",
        "aquarius-cp",
        "--allow-newer-protocol",
        "--fixture",
        CAPTURE_P29,
        "--format",
        "json",
    ]);

    assert_success(&output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.starts_with("warning: --allow-newer-protocol: executing Protocol 29 ledger"));
    assert!(stderr.contains("on the Protocol 28 Host"));

    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ledger"], 64_731_471);
    assert_eq!(report["networkProtocol"], 29);
    assert_eq!(report["executedProtocol"], 28);
    // Identical to Kanatoko 29 executing the same capture on a Protocol 29 Host.
    assert_eq!(report["quoteBefore"], "44629339");
    assert_eq!(report["quoteAfter"], "36888783");
    assert_eq!(report["restoredQuote"], "44629339");
    assert_eq!(report["unknownFailClosed"], true);
    assert_eq!(report["upstreamReads"], 0);
    let receipts = report["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 8);
    for receipt in receipts {
        assert_eq!(receipt["networkProtocol"], 29);
        assert_eq!(receipt["executedProtocol"], 28);
        assert_eq!(receipt["upstreamReads"], 0);
    }
}

fn run_offline(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kanatoko"))
        .args(args)
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
