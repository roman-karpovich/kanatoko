//! In-process offline replay on the snapshot protocol. Contract calls cross the SDK boundary as XDR.
use std::path::Path;

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
    let file = std::fs::File::open(path)
        .unwrap_or_else(|error| panic!("cannot open {}: {error}", path.display()));
    let header: Header = serde_json::from_reader(std::io::BufReader::new(file))
        .unwrap_or_else(|error| panic!("invalid capture {}: {error}", path.display()));
    match header.provenance.protocol_version {
        29 => p29::run(path, contracts, scenario),
        30 => p30::run(path, contracts, scenario),
        version => panic!("unsupported snapshot protocol {version:?}"),
    }
}

#[derive(serde::Deserialize)]
struct Header {
    provenance: Provenance,
}

#[derive(serde::Deserialize)]
struct Provenance {
    protocol_version: u32,
}

mod p29 {
    use engine29 as engine;
    include!("backend.rs");
}
mod p30 {
    use crate as engine;
    include!("backend.rs");
}
