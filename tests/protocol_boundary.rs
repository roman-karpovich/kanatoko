#![cfg(all(feature = "capture", not(kanatoko_protocol_27_fixtures)))]

use kanatoko::{CaptureError, CapturedFixture, FixtureError, SUPPORTED_PROTOCOL_VERSION};

const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const PROTOCOL_27_CAPTURE: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/capture-p27.json";

#[test]
fn protocol_27_capture_fails_closed_on_a_different_protocol_host() {
    let error = CapturedFixture::from_file(PROTOCOL_27_CAPTURE, MAINNET_PASSPHRASE).unwrap_err();

    assert!(matches!(
        error,
        CaptureError::Fixture(FixtureError::UnsupportedProtocol {
            found: 27,
            supported,
        }) if supported == SUPPORTED_PROTOCOL_VERSION
    ));
}

#[cfg(not(kanatoko_protocol_28_fixtures))]
mod protocol_28 {
    use super::{
        CaptureError, CapturedFixture, FixtureError, MAINNET_PASSPHRASE, SUPPORTED_PROTOCOL_VERSION,
    };

    const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";
    const MAINNET_CAPTURE: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/capture-p28.json";
    const TESTNET_CAPTURE: &str = "fixtures/testnet/native-xlm-p28/auto-capture.json";

    #[test]
    fn protocol_28_mainnet_capture_fails_closed_on_a_different_protocol_host() {
        assert_rejected_as_protocol_28(
            &CapturedFixture::from_file(MAINNET_CAPTURE, MAINNET_PASSPHRASE).unwrap_err(),
        );
    }

    #[test]
    fn protocol_28_testnet_capture_fails_closed_on_a_different_protocol_host() {
        assert_rejected_as_protocol_28(
            &CapturedFixture::from_file(TESTNET_CAPTURE, TESTNET_PASSPHRASE).unwrap_err(),
        );
    }

    fn assert_rejected_as_protocol_28(error: &CaptureError) {
        assert!(matches!(
            error,
            CaptureError::Fixture(FixtureError::UnsupportedProtocol {
                found: 28,
                supported,
            }) if *supported == SUPPORTED_PROTOCOL_VERSION
        ));
    }
}
