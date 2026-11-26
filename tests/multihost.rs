#![cfg(feature = "multihost")]

use std::path::Path;

use kanatoko::multihost::replay;
use soroban_sdk::{
    xdr::{FromXdr, ToXdr},
    Address, Bytes, Env, Val, Vec,
};

const PAIR: &str = "CAM7DY53G63XA4AJRS24Z6VFYAFSSF76C3RZ45BE5YU3FQS5255OOABP";
const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

#[test]
fn reads_the_same_soroswap_pair_on_both_protocols() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainnet/soroswap-xlm-usdc");
    let codec = Env::default();
    let args: std::vec::Vec<_> = Vec::<Val>::new(&codec).to_xdr(&codec).iter().collect();
    for (protocol, file) in [(29, "auto-capture-p29.json"), (30, "auto-capture.json")] {
        let result = replay(&fixtures.join(file), &[(PAIR, None)], |fork| {
            assert_eq!(fork.protocol_version(), protocol);
            fork.call(PAIR, "token_1", &args).unwrap()
        });
        let token = Address::from_xdr(&codec, &Bytes::from_slice(&codec, &result)).unwrap();
        assert_eq!(token, Address::from_str(&codec, USDC));
    }
}
