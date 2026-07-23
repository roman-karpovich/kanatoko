#![cfg(all(feature = "capture", not(kanatoko_protocol_27_fixtures)))]

use kanatoko::{CaptureError, CapturedFixture, FixtureError, SUPPORTED_PROTOCOL_VERSION};

const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const PROTOCOL_27_CAPTURE: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/capture.json";

#[test]
fn protocol_27_capture_fails_closed_on_an_older_host() {
    let error = CapturedFixture::from_file(PROTOCOL_27_CAPTURE, MAINNET_PASSPHRASE).unwrap_err();

    assert!(matches!(
        error,
        CaptureError::Fixture(FixtureError::UnsupportedProtocol {
            found: 27,
            supported,
        }) if supported == SUPPORTED_PROTOCOL_VERSION
    ));
}
