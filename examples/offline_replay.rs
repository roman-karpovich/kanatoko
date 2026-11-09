//! Replays the committed Soroswap XLM/USDC Mainnet capture and prints a quote.
//!
//! The capture is loaded from disk and replayed with `.offline()`, which never
//! contacts an RPC endpoint. Run it with:
//!
//! ```sh
//! cargo run --locked --features capture --example offline_replay
//! ```

use kanatoko::soroban_sdk::{vec, Vec};
use kanatoko::{mainnet, CacheStatus, PreviewAuth};

const BUNDLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/mainnet/soroswap-xlm-usdc/capture.json"
);
const ROUTER: &str = "CAG5LRYQ5JVEUI5TEID72EYOVX44TTUJT5BQR2J6J77FH65PCCFAJDDH";
const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const ONE_USDC: i128 = 10_000_000;

fn main() {
    let run = mainnet()
        .cache(BUNDLE)
        .offline()
        .run(|fork| {
            let router = fork.contract(ROUTER);
            let path = vec![fork.env(), fork.contract(USDC), fork.contract(XLM)];
            let report = fork
                .preview(
                    &router,
                    "router_get_amounts_out",
                    (ONE_USDC, path),
                    PreviewAuth::Record,
                )
                .expect("the quote must replay from the capture");
            let amounts: Vec<i128> = report
                .result(fork.env())
                .expect("the router must return amounts");

            let upstream_reads = report.receipt().upstream_reads;
            assert_eq!(upstream_reads, 0, "offline replay must not use RPC");
            println!("quote: 1 USDC -> {} XLM stroops", amounts.get_unchecked(1));
            println!("upstream RPC reads: {upstream_reads}");
        })
        .expect("offline replay must succeed");

    assert_eq!(run.cache_status(), CacheStatus::Hit);
    println!("ledger: {}", run.fixture().provenance().ledger_sequence());
}
