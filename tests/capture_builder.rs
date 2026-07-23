#![cfg(feature = "capture")]

use kanatoko::CaptureBuilder;

const ROOT: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";

#[test]
fn builder_debug_redacts_rpc_credentials_path_and_query() {
    let secret = "do-not-leak";
    let builder = CaptureBuilder::mainnet(
        format!("https://user:{secret}@rpc.example.test/private/{secret}?token={secret}"),
        ROOT,
    )
    .unwrap();

    let debug = format!("{builder:?}");
    assert!(debug.contains("https://rpc.example.test"));
    assert!(!debug.contains(secret));
    assert!(!debug.contains("private"));
    assert!(!debug.contains("user:"));
}
