use super::Session;
use engine::soroban_sdk::{
    testutils::Ledger,
    xdr::{self, FromXdr, ToXdr},
    Address, Bytes, Env, Val, Vec as Args,
};
use engine::{canonical_ledger_digest, CapturedFixture, ScenarioFork};
use sha2::{Digest, Sha256};
use std::path::Path;

const NETWORK: &str = "Public Global Stellar Network ; September 2015";

pub(super) fn run<T>(
    path: &Path,
    pins: &[(&str, Option<&str>)],
    scenario: impl Fn(&dyn Session) -> T,
) -> T {
    let capture = CapturedFixture::from_file(path, NETWORK).unwrap();
    let protocol = capture.provenance().protocol_version();
    let snapshot = capture.frozen_fixture().ledger_snapshot();
    let env = Env::default();
    for &(id, pinned) in pins {
        let address = xdr::ScAddress::from(&Address::from_str(&env, id));
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

    assert_eq!(engine::SUPPORTED_PROTOCOL_VERSION, protocol);
    assert_eq!(capture.report().final_replay_rpc_reads(), 0);
    capture
        .replay_scenario(|fork| {
            assert_eq!(fork.protocol_version(), protocol);
            scenario(fork)
        })
        .expect("offline scenario replay failed")
}

impl Session for ScenarioFork<'_> {
    fn call(&self, contract: &str, function: &str, args: &[u8]) -> Result<Vec<u8>, String> {
        let env = self.env();
        let args = Args::<Val>::from_xdr(env, &Bytes::from_slice(env, args))
            .map_err(|_| "invalid XDR argument vector".to_owned())?;
        self.try_invoke::<Val>(&self.contract(contract), function, args)
            .map(|v| v.to_xdr(env).iter().collect())
            .map_err(|failure| format!("{:?}", failure.kind))
    }
    fn mock_funded_source_account(&self, label: &str, stroops: i64) -> String {
        self.mock_all_auths();
        let account = self.local_account(label);
        self.fund_local_account(&account, stroops);
        let xdr::ScAddress::Account(id) = xdr::ScAddress::from(&account) else {
            unreachable!()
        };
        self.env().host().set_source_account(id).unwrap();
        account.to_string().to_string()
    }
    fn protocol_version(&self) -> u32 {
        self.env()
            .host()
            .with_ledger_info(|ledger| Ok(ledger.protocol_version))
            .unwrap()
    }
    fn timestamp(&self) -> u64 {
        self.env().ledger().timestamp()
    }
    fn set_timestamp(&self, t: u64) {
        self.env().ledger().set_timestamp(t);
    }
    fn replace_wasm(&self, contract: &str, wasm: &[u8]) -> [u8; 32] {
        self.replace_wasm(&self.contract(contract), wasm)
    }
    fn digest(&self) -> [u8; 32] {
        canonical_ledger_digest(&self.env().to_ledger_snapshot()).unwrap()
    }
    fn storage_digest(&self, replaced_contract: &str) -> [u8; 32] {
        let address = xdr::ScAddress::from(&self.contract(replaced_contract));
        let mut snapshot = self.env().to_ledger_snapshot();
        snapshot
            .ledger_entries
            .retain(|(key, _)| !matches!(key.as_ref(), xdr::LedgerKey::ContractCode(_)));
        for (_, (entry, _)) in &mut snapshot.ledger_entries {
            if let xdr::LedgerEntryData::ContractData(data) = &mut entry.data {
                if data.contract == address && data.key == xdr::ScVal::LedgerKeyContractInstance {
                    let xdr::ScVal::ContractInstance(instance) = &mut data.val else {
                        unreachable!()
                    };
                    instance.executable = xdr::ContractExecutable::Wasm(xdr::Hash([0; 32]));
                }
            }
        }
        canonical_ledger_digest(&snapshot).unwrap()
    }
}
