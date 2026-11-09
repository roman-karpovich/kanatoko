#![cfg(all(feature = "capture", not(kanatoko_protocol_27_fixtures)))]

use kanatoko::{CaptureError, CapturedFixture, FixtureError, SUPPORTED_PROTOCOL_VERSION};

const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const PROTOCOL_27_CAPTURE: &str = "fixtures/synthetic/mainnet-protocol-27.capture.json";

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

/// Bundles written by this release keep the established bundle format:
/// re-writing the committed Protocol 29 bundles reproduces them byte for byte,
/// whether they were loaded with or without the newer-protocol opt-in.
#[cfg(kanatoko_protocol_29_fixtures)]
#[test]
fn committed_bundles_rewrite_byte_identically() {
    const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";
    assert_bundles_rewrite_byte_identically(&[
        (
            "fixtures/mainnet/soroswap-xlm-usdc/capture.json",
            MAINNET_PASSPHRASE,
        ),
        (
            "fixtures/mainnet/soroswap-xlm-usdc/auto-capture.json",
            MAINNET_PASSPHRASE,
        ),
        (
            "fixtures/testnet/native-xlm-p29/auto-capture.json",
            TESTNET_PASSPHRASE,
        ),
    ]);
}

/// Re-writing the committed Protocol 30 bundle reproduces it byte for byte,
/// whether it was loaded with or without the newer-protocol opt-in.
#[cfg(kanatoko_protocol_30_fixtures)]
#[test]
fn committed_bundles_rewrite_byte_identically() {
    assert_bundles_rewrite_byte_identically(&[(
        "fixtures/standalone/native-xlm-stateful-p30/capture.json",
        "Standalone Network ; February 2017",
    )]);
}

#[cfg(any(kanatoko_protocol_29_fixtures, kanatoko_protocol_30_fixtures))]
fn assert_bundles_rewrite_byte_identically(bundles: &[(&str, &str)]) {
    let loaded = bundles.iter().flat_map(|(path, passphrase)| {
        [
            (*path, CapturedFixture::from_file(path, passphrase)),
            (
                *path,
                CapturedFixture::from_file_allowing_newer_protocol(path, passphrase),
            ),
        ]
    });
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

#[cfg(not(kanatoko_protocol_28_fixtures))]
mod protocol_28 {
    use kanatoko::{mainnet, testnet, AutoRunError};

    use super::{
        CaptureError, CapturedFixture, FixtureError, MAINNET_PASSPHRASE, SUPPORTED_PROTOCOL_VERSION,
    };

    const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";
    const MAINNET_CAPTURE: &str = "fixtures/synthetic/mainnet-protocol-28.capture.json";
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

    /// The newer-protocol opt-in accepts only the protocol after the Host's
    /// own; an older capture still fails closed.
    #[test]
    fn protocol_28_captures_fail_closed_even_with_the_newer_protocol_opt_in() {
        for (path, passphrase) in [
            (MAINNET_CAPTURE, MAINNET_PASSPHRASE),
            (TESTNET_CAPTURE, TESTNET_PASSPHRASE),
        ] {
            assert_rejected_as_protocol_28(
                &CapturedFixture::from_file_allowing_newer_protocol(path, passphrase).unwrap_err(),
            );
        }
    }

    #[test]
    fn protocol_28_offline_runners_fail_closed_with_and_without_the_opt_in() {
        for result in [
            mainnet()
                .cache(MAINNET_CAPTURE)
                .offline()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
            mainnet()
                .cache(MAINNET_CAPTURE)
                .offline()
                .allow_newer_protocol()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
            testnet()
                .cache(TESTNET_CAPTURE)
                .offline()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
            testnet()
                .cache(TESTNET_CAPTURE)
                .offline()
                .allow_newer_protocol()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
        ] {
            let Err(AutoRunError::Capture(error)) = result else {
                panic!("a Protocol 28 cache must be rejected");
            };
            assert_rejected_as_protocol_28(&error);
        }
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

#[cfg(kanatoko_protocol_30_fixtures)]
mod protocol_29 {
    use std::process::Command;

    use kanatoko::{mainnet, testnet, AutoRunError};

    use super::{
        CaptureError, CapturedFixture, FixtureError, MAINNET_PASSPHRASE, SUPPORTED_PROTOCOL_VERSION,
    };

    const TESTNET_PASSPHRASE: &str = "Test SDF Network ; September 2015";
    const MAINNET_CAPTURE: &str = "fixtures/mainnet/soroswap-xlm-usdc/capture.json";
    const MAINNET_AUTO_CAPTURE: &str = "fixtures/mainnet/soroswap-xlm-usdc/auto-capture.json";
    const TESTNET_CAPTURE: &str = "fixtures/testnet/native-xlm-p29/auto-capture.json";
    const CAPTURES: [(&str, &str); 3] = [
        (MAINNET_CAPTURE, MAINNET_PASSPHRASE),
        (MAINNET_AUTO_CAPTURE, MAINNET_PASSPHRASE),
        (TESTNET_CAPTURE, TESTNET_PASSPHRASE),
    ];

    #[test]
    fn protocol_29_captures_fail_closed_on_a_different_protocol_host() {
        for (path, passphrase) in CAPTURES {
            assert_rejected_as_protocol_29(
                &CapturedFixture::from_file(path, passphrase).unwrap_err(),
            );
        }
    }

    /// The newer-protocol opt-in accepts only the protocol after the Host's
    /// own; the preceding protocol still fails closed.
    #[test]
    fn protocol_29_captures_fail_closed_even_with_the_newer_protocol_opt_in() {
        for (path, passphrase) in CAPTURES {
            assert_rejected_as_protocol_29(
                &CapturedFixture::from_file_allowing_newer_protocol(path, passphrase).unwrap_err(),
            );
        }
    }

    #[test]
    fn protocol_29_offline_runners_fail_closed_with_and_without_the_opt_in() {
        for result in [
            mainnet()
                .cache(MAINNET_AUTO_CAPTURE)
                .offline()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
            mainnet()
                .cache(MAINNET_AUTO_CAPTURE)
                .offline()
                .allow_newer_protocol()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
            testnet()
                .cache(TESTNET_CAPTURE)
                .offline()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
            testnet()
                .cache(TESTNET_CAPTURE)
                .offline()
                .allow_newer_protocol()
                .run(|_| unreachable!("a rejected cache must not run the scenario")),
        ] {
            let Err(AutoRunError::Capture(error)) = result else {
                panic!("a Protocol 29 cache must be rejected");
            };
            assert_rejected_as_protocol_29(&error);
        }
    }

    /// The CLI's default fixture is the Protocol 29 Soroswap capture.
    #[test]
    fn cli_rejects_the_default_protocol_29_fixture_with_and_without_the_opt_in() {
        for args in [
            &["run", "soroswap-xlm-usdc"][..],
            &["run", "soroswap-xlm-usdc", "--allow-newer-protocol"][..],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_kanatoko"))
                .args(args)
                .env("HTTP_PROXY", "http://127.0.0.1:9")
                .env("HTTPS_PROXY", "http://127.0.0.1:9")
                .env("ALL_PROXY", "http://127.0.0.1:9")
                .env("NO_PROXY", "")
                .output()
                .unwrap();

            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                stderr.contains(&format!(
                    "UnsupportedProtocol {{ found: 29, supported: {SUPPORTED_PROTOCOL_VERSION} }}"
                )),
                "{stderr}"
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
}
