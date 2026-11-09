//! Replays the committed standalone Protocol 30 capture and reads a contract.
//!
//! No public network runs Protocol 30 yet, so this line replays a capture of a
//! local standalone network. The capture is loaded from disk into a strict
//! fork, which holds no RPC transport. Run it with:
//!
//! ```sh
//! cargo run --locked --features capture --example offline_replay
//! ```

use kanatoko::soroban_env_host::xdr::{ScAddress, ScVal};
use kanatoko::{AuthMode, CapturedFixture, ExecutionMode, InvokeOutcome, InvokeRequest};

const BUNDLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/standalone/native-xlm-stateful-p30/capture.json"
);
const STANDALONE_PASSPHRASE: &str = "Standalone Network ; February 2017";
const STATEFUL: &str = "CCNHIRURTVMTX65DOASMBGJSQZLZMFGRO7QBHMBDSFPGEQ7AWH6VMTH5";

fn main() {
    let captured = CapturedFixture::from_file(BUNDLE, STANDALONE_PASSPHRASE)
        .expect("the committed capture must load");
    let mut fork = captured.fork();

    let contract: ScAddress = STATEFUL.parse().expect("a valid contract address");
    let function = "get".try_into().expect("a valid function name");
    let receipt = fork
        .invoke(
            InvokeRequest {
                contract,
                function,
                args: vec![],
            },
            ExecutionMode::Preview,
            AuthMode::Enforce(vec![]),
        )
        .expect("the call must replay from the capture");

    let InvokeOutcome::Success(ScVal::I64(value)) = receipt.outcome else {
        panic!("unexpected outcome: {:?}", receipt.outcome);
    };
    assert_eq!(receipt.upstream_reads, 0, "offline replay must not use RPC");
    println!("stateful contract value: {value}");
    println!("upstream RPC reads: {}", receipt.upstream_reads);
    println!("ledger: {}", captured.provenance().ledger_sequence());
}
