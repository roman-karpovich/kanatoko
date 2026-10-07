#![cfg(all(feature = "capture", kanatoko_protocol_29_fixtures))]

use std::process::{Command, Output};

use serde_json::Value;

const CAPTURE_P28: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/capture-p28.json";

#[test]
fn cli_runs_the_strict_aquarius_workflow_offline() {
    let output = run_offline(&["run", "aquarius-cp", "--format", "text"]);

    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("strict Aquarius run: ok"));
    assert!(stdout.contains("ledger: 64731471"));
    assert!(stdout.contains("quote before: 44629339"));
    assert!(stdout.contains("quote after: 36888783"));
    assert!(stdout.contains("revert restored quote: 44629339"));
    assert!(stdout.contains("unknown key fail-closed: true"));
    assert!(stdout.contains("upstream reads: 0"));
    assert!(stdout.contains("not transaction-faithful deploy"));
    // Without the opt-in the output carries no protocol traceability lines.
    assert!(!stdout.contains("protocol"));
    assert!(output.stderr.is_empty());
}

#[test]
fn cli_rejects_a_protocol_28_fixture_with_and_without_the_opt_in() {
    for args in [
        &["run", "aquarius-cp", "--fixture", CAPTURE_P28][..],
        &[
            "run",
            "aquarius-cp",
            "--allow-newer-protocol",
            "--fixture",
            CAPTURE_P28,
        ][..],
    ] {
        let output = run_offline(args);

        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("UnsupportedProtocol { found: 28, supported: 29 }"),
            "{stderr}"
        );
    }
}

/// The opt-in only adds protocol traceability to a fixture at the Host's own
/// protocol; there is nothing to downgrade, so no warning is printed.
#[test]
fn cli_opt_in_on_a_current_protocol_fixture_reports_both_protocols_without_a_warning() {
    let output = run_offline(&[
        "run",
        "aquarius-cp",
        "--allow-newer-protocol",
        "--format",
        "json",
    ]);

    assert_success(&output);
    assert!(output.stderr.is_empty());

    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ledger"], 64_731_471);
    assert_eq!(report["networkProtocol"], 29);
    assert_eq!(report["executedProtocol"], 29);
    assert_eq!(report["quoteBefore"], "44629339");
    assert_eq!(report["quoteAfter"], "36888783");
    assert_eq!(report["restoredQuote"], "44629339");
    assert_eq!(report["unknownFailClosed"], true);
    assert_eq!(report["upstreamReads"], 0);
    let receipts = report["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 8);
    for receipt in receipts {
        assert_eq!(receipt["networkProtocol"], 29);
        assert_eq!(receipt["executedProtocol"], 29);
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
