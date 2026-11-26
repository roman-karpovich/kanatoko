//! In-process offline replay on the snapshot protocol. Contract calls cross the SDK boundary as XDR.
use crate::soroban_ledger_snapshot::LedgerSnapshot;
use crate::soroban_sdk::xdr;
use crate::{fixture::ProtocolPolicy, CapturedFixture};
use sha2::{Digest, Sha256};
use std::path::Path;

mod current;
#[cfg(feature = "host-29")]
mod historical;

/// A single mutable, offline fork. XDR arguments encode one Soroban vector.
pub trait Session {
    /// Invoke a captured contract with an XDR-encoded vector of arguments.
    ///
    /// # Errors
    ///
    /// Returns a decoding error or the Host's invocation failure kind.
    fn call(&self, contract: &str, function: &str, args: &[u8]) -> Result<Vec<u8>, String>;
    /// Enable mocked auth, fund a deterministic local account, and select it as the source.
    fn mock_funded_source_account(&self, label: &str, stroops: i64) -> String;
    fn replace_wasm(&self, contract: &str, wasm: &[u8]) -> [u8; 32];
    /// Ledger protocol reported by the executing Host.
    fn protocol_version(&self) -> u32;
    fn timestamp(&self) -> u64;
    fn set_timestamp(&self, timestamp: u64);
    fn digest(&self) -> [u8; 32];
    /// Hash ledger state and TTLs, excluding code entries and the replaced executable hash.
    fn storage_digest(&self, replaced_contract: &str) -> [u8; 32];
}

/// Replay a saved mainnet capture with its matching Host. Contract pins are optional SHA-256 hex.
///
/// # Panics
///
/// Panics if the fixture cannot be read, its protocol is unsupported, a pin is invalid,
/// or the scenario fails strict replay.
pub fn replay<T>(
    path: &Path,
    contracts: &[(&str, Option<&str>)],
    scenario: impl Fn(&dyn Session) -> T,
) -> T {
    let capture = CapturedFixture::from_file_with_policy(
        path,
        crate::capture::MAINNET_PASSPHRASE,
        ProtocolPolicy::ReplayHosts,
    )
    .unwrap_or_else(|error| {
        #[cfg(not(feature = "host-29"))]
        if let crate::CaptureError::Fixture(crate::FixtureError::UnsupportedProtocol {
            found: 29,
            ..
        }) = error
        {
            panic!(
                "enable Kanatoko's host-29 feature to replay {}",
                path.display()
            );
        }
        panic!("invalid mainnet capture {}: {error}", path.display())
    });
    check_pins(capture.frozen_fixture().ledger_snapshot(), contracts);
    #[cfg(feature = "host-29")]
    if capture.provenance().protocol_version() == host29::VERSION.interface.protocol {
        return historical::run(&capture, scenario);
    }
    capture
        .replay_scenario(|fork| scenario(fork))
        .expect("offline scenario replay failed")
}

fn check_pins(snapshot: &LedgerSnapshot, pins: &[(&str, Option<&str>)]) {
    for &(id, pinned) in pins {
        let address = id
            .parse::<xdr::ScAddress>()
            .expect("invalid contract address");
        let hash = snapshot
            .ledger_entries
            .iter()
            .find_map(|(_, (entry, _))| {
                let xdr::LedgerEntryData::ContractData(data) = &entry.data else {
                    return None;
                };
                if data.contract != address || data.key != xdr::ScVal::LedgerKeyContractInstance {
                    return None;
                }
                let xdr::ScVal::ContractInstance(instance) = &data.val else {
                    return None;
                };
                let xdr::ContractExecutable::Wasm(hash) = &instance.executable else {
                    return None;
                };
                Some(hash)
            })
            .expect("missing contract instance");
        let code = snapshot
            .ledger_entries
            .iter()
            .find_map(|(_, (entry, _))| {
                let xdr::LedgerEntryData::ContractCode(code) = &entry.data else {
                    return None;
                };
                (code.hash == *hash).then_some(code)
            })
            .expect("missing contract code");
        let digest = Sha256::digest(code.code.as_slice());
        assert_eq!(<[u8; 32]>::from(digest), hash.0);
        if let Some(expected) = pinned {
            assert_eq!(format!("{digest:x}"), expected);
        }
    }
}

fn storage_digest(mut snapshot: LedgerSnapshot, address: &xdr::ScAddress) -> [u8; 32] {
    snapshot
        .ledger_entries
        .retain(|(key, _)| !matches!(key.as_ref(), xdr::LedgerKey::ContractCode(_)));
    for (_, (entry, _)) in &mut snapshot.ledger_entries {
        if let xdr::LedgerEntryData::ContractData(data) = &mut entry.data {
            if &data.contract == address && data.key == xdr::ScVal::LedgerKeyContractInstance {
                let xdr::ScVal::ContractInstance(instance) = &mut data.val else {
                    unreachable!()
                };
                instance.executable = xdr::ContractExecutable::Wasm(xdr::Hash([0; 32]));
            }
        }
    }
    crate::canonical_ledger_digest(&snapshot).unwrap()
}
