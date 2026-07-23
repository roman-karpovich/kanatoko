#![cfg(all(feature = "capture", kanatoko_protocol_27_fixtures))]

use std::process::Command;

#[test]
fn cli_runs_the_strict_aquarius_workflow_offline() {
    let output = Command::new(env!("CARGO_BIN_EXE_kanatoko"))
        .args(["run", "aquarius-cp", "--format", "text"])
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("strict Aquarius run: ok"));
    assert!(stdout.contains("unknown key fail-closed: true"));
    assert!(stdout.contains("upstream reads: 0"));
    assert!(stdout.contains("not transaction-faithful deploy"));
}
