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

#[test]
fn protocol_27_capture_fails_closed_even_with_the_newer_protocol_opt_in() {
    let error =
        CapturedFixture::from_file_allowing_newer_protocol(PROTOCOL_27_CAPTURE, MAINNET_PASSPHRASE)
            .unwrap_err();

    assert!(matches!(
        error,
        CaptureError::Fixture(FixtureError::UnsupportedProtocol {
            found: 27,
            supported,
        }) if supported == SUPPORTED_PROTOCOL_VERSION
    ));
}

/// Bundles written by this release keep the 28.0.0 format: re-writing the
/// committed Protocol 28 bundles and, with the opt-in, the Protocol 29 bundles
/// written by the 29 line reproduces them byte for byte.
#[cfg(kanatoko_protocol_28_fixtures)]
#[test]
fn committed_bundles_rewrite_byte_identically() {
    let protocol_28 = [
        "fixtures/mainnet/aquarius-xlm-usdc-cp/capture.json",
        "fixtures/mainnet/aquarius-xlm-usdc-cp/auto-capture.json",
    ];
    let protocol_29 = [
        "fixtures/mainnet/aquarius-xlm-usdc-cp/capture-p29.json",
        "fixtures/mainnet/aquarius-xlm-usdc-cp/auto-capture-p29.json",
    ];
    let loaded = protocol_28
        .iter()
        .map(|path| (*path, CapturedFixture::from_file(path, MAINNET_PASSPHRASE)))
        .chain(protocol_29.iter().map(|path| {
            (
                *path,
                CapturedFixture::from_file_allowing_newer_protocol(path, MAINNET_PASSPHRASE),
            )
        }));
    for (index, (path, captured)) in loaded.enumerate() {
        let output = std::env::temp_dir().join(format!(
            "kanatoko-rewrite-{}-{index}.capture.json",
            std::process::id()
        ));
        captured.unwrap().write_file(&output).unwrap();
        let rewritten = std::fs::read(&output).unwrap();
        std::fs::remove_file(&output).unwrap();
        assert_eq!(rewritten, std::fs::read(path).unwrap(), "{path}");
    }
}

#[cfg(kanatoko_protocol_28_fixtures)]
mod protocol_29 {
    use kanatoko::{mainnet, testnet, AutoRunError, CacheStatus};

    use super::{
        CaptureError, CapturedFixture, FixtureError, MAINNET_PASSPHRASE, SUPPORTED_PROTOCOL_VERSION,
    };

    const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";
    const MAINNET_CAPTURE: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp/capture-p29.json";
    const MAINNET_AUTO_CAPTURE: &str =
        "fixtures/mainnet/aquarius-xlm-usdc-cp/auto-capture-p29.json";
    const TESTNET_CAPTURE: &str = "fixtures/testnet/native-xlm-p29/auto-capture.json";

    #[test]
    fn protocol_29_captures_fail_closed_without_the_opt_in() {
        for (path, passphrase) in [
            (MAINNET_CAPTURE, MAINNET_PASSPHRASE),
            (MAINNET_AUTO_CAPTURE, MAINNET_PASSPHRASE),
            (TESTNET_CAPTURE, TESTNET_PASSPHRASE),
        ] {
            assert_rejected_as_protocol_29(
                &CapturedFixture::from_file(path, passphrase).unwrap_err(),
            );
        }
    }

    #[test]
    fn protocol_29_offline_runners_fail_closed_without_the_opt_in() {
        for result in [
            mainnet()
                .cache(MAINNET_AUTO_CAPTURE)
                .offline()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
            testnet()
                .cache(TESTNET_CAPTURE)
                .offline()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
        ] {
            let Err(AutoRunError::Capture(error)) = result else {
                panic!("a Protocol 29 cache must be rejected without the opt-in");
            };
            assert_rejected_as_protocol_29(&error);
        }
    }

    #[test]
    fn protocol_29_captures_load_with_the_opt_in_and_report_both_protocols() {
        for (path, passphrase) in [
            (MAINNET_CAPTURE, MAINNET_PASSPHRASE),
            (MAINNET_AUTO_CAPTURE, MAINNET_PASSPHRASE),
            (TESTNET_CAPTURE, TESTNET_PASSPHRASE),
        ] {
            let captured =
                CapturedFixture::from_file_allowing_newer_protocol(path, passphrase).unwrap();
            assert_eq!(captured.provenance().protocol_version(), 29);
            assert_eq!(
                captured.provenance().executed_protocol_version(),
                SUPPORTED_PROTOCOL_VERSION
            );
            assert_eq!(
                captured.frozen_fixture().ledger_snapshot().protocol_version,
                29
            );
            let fork = captured.fork();
            assert_eq!(fork.network_protocol_version(), 29);
            assert_eq!(fork.executed_protocol_version(), SUPPORTED_PROTOCOL_VERSION);
            assert_eq!(
                fork.ledger_digest().unwrap(),
                captured.frozen_fixture().ledger_digest()
            );
        }
    }

    fn assert_rejected_as_protocol_29(error: &CaptureError) {
        assert!(matches!(
            error,
            CaptureError::Fixture(FixtureError::UnsupportedProtocol {
                found: 29,
                supported,
            }) if *supported == SUPPORTED_PROTOCOL_VERSION
        ));
    }

    #[test]
    fn protocol_29_offline_runner_hits_the_cache_with_the_opt_in() {
        let run = mainnet()
            .cache(MAINNET_AUTO_CAPTURE)
            .offline()
            .allow_newer_protocol()
            .run(|fork| {
                assert_eq!(fork.network_protocol_version(), 29);
                assert_eq!(fork.executed_protocol_version(), SUPPORTED_PROTOCOL_VERSION);
            })
            .unwrap();
        assert_eq!(run.cache_status(), CacheStatus::Hit);
    }
}
