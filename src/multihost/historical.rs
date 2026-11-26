//! Protocol 29 runs on Stellar's published Host. Only canonical XDR crosses
//! the version boundary between this Host and Kanatoko's capture inventory.
use super::Session;
use crate::{
    auto::{funded_account_entry, local_public_key},
    capture::{contract_instance_key, LocalLedger, TrackingSource},
    soroban_env_host::{storage::SnapshotSource as _, xdr},
    soroban_ledger_snapshot::LedgerSnapshot,
    CapturedFixture,
};
use host29::{
    storage::{EntryWithLiveUntil, SnapshotSource, Storage},
    xdr::{self as old, HostFunction, InvokeContractArgs},
    Host, HostError, LedgerInfo,
};
use sha2::{Digest, Sha256};
use std::rc::Rc;

fn historical<T: old::ReadXdr>(value: &impl xdr::WriteXdr) -> T {
    T::from_xdr(
        value.to_xdr(xdr::Limits::none()).unwrap(),
        old::Limits::none(),
    )
    .expect("value must be representable in Protocol 29 XDR")
}

fn current<T: xdr::ReadXdr>(value: &impl old::WriteXdr) -> T {
    T::from_xdr(
        value.to_xdr(old::Limits::none()).unwrap(),
        xdr::Limits::none(),
    )
    .expect("invalid Protocol 29 XDR")
}

struct Source(Rc<TrackingSource>);

impl SnapshotSource for Source {
    fn get(&self, key: &Rc<old::LedgerKey>) -> Result<Option<EntryWithLiveUntil>, HostError> {
        self.0
            .get(&Rc::new(current(&**key)))
            .map(|e| e.map(|(entry, ttl)| (Rc::new(historical(&*entry)), ttl)))
            .map_err(|_| (old::ScErrorType::Storage, old::ScErrorCode::InternalError).into())
    }
}

struct Fork<'a> {
    host: Host,
    snapshot: &'a LedgerSnapshot,
    local: Rc<LocalLedger>,
}

pub(super) fn run<T>(capture: &CapturedFixture, scenario: impl Fn(&dyn Session) -> T) -> T {
    let snapshot = capture.frozen_fixture().ledger_snapshot();
    let local = Rc::new(LocalLedger::default());
    let source = capture.replay_source(local.clone());
    let storage = Storage::with_recording_footprint(Rc::new(Source(source.clone())));
    let host = Host::with_storage_and_budget(storage, host29::budget::Budget::default());
    assert_eq!(
        snapshot.protocol_version,
        host29::VERSION.interface.protocol
    );
    host.set_ledger_info(LedgerInfo {
        protocol_version: snapshot.protocol_version,
        sequence_number: snapshot.sequence_number,
        timestamp: snapshot.timestamp,
        network_id: snapshot.network_id,
        base_reserve: snapshot.base_reserve,
        min_persistent_entry_ttl: snapshot.min_persistent_entry_ttl,
        min_temp_entry_ttl: snapshot.min_temp_entry_ttl,
        max_entry_ttl: snapshot.max_entry_ttl,
    })
    .unwrap();
    host.set_source_account(old::AccountId(old::PublicKey::PublicKeyTypeEd25519(
        old::Uint256([0; 32]),
    )))
    .unwrap();
    host.set_base_prng_seed([0; 32]).unwrap();
    host.enable_invocation_metering();
    host.set_shadow_budget_limits(u64::MAX, u64::MAX).unwrap();
    let fork = Fork {
        host,
        snapshot,
        local,
    };
    let result = scenario(&fork);
    let keys: Vec<xdr::LedgerKey> = fork
        .host
        .get_stored_entries()
        .unwrap()
        .iter()
        .map(|(k, _)| current(&**k))
        .collect();
    capture
        .verify_replay_keys(&source, &fork.local, keys.iter())
        .expect("scenario touched uncaptured ledger keys");
    result
}

impl Fork<'_> {
    fn state(&self) -> LedgerSnapshot {
        let mut state = self.snapshot.clone();
        state.timestamp = self.timestamp();
        for (key, stored) in self.host.get_stored_entries().unwrap() {
            let change = (
                Rc::new(current(&*key)),
                stored.map(|(entry, ttl)| (Rc::new(current(&*entry)), ttl)),
            );
            state.update_entries(std::iter::once(&change));
        }
        state
    }
}

impl Session for Fork<'_> {
    fn call(&self, contract: &str, function: &str, args: &[u8]) -> Result<Vec<u8>, String> {
        use old::{ReadXdr as _, WriteXdr as _};
        let Ok(old::ScVal::Vec(Some(args))) = old::ScVal::from_xdr(args, old::Limits::none())
        else {
            return Err("invalid XDR argument vector".into());
        };
        let invocation = InvokeContractArgs {
            contract_address: contract.parse().map_err(|_| "invalid contract address")?,
            function_name: function.try_into().map_err(|()| "invalid function name")?,
            args: args.0,
        };
        let value = self
            .host
            .invoke_function(HostFunction::InvokeContract(invocation))
            .map_err(|failure| {
                if failure.error.is_type(old::ScErrorType::Contract) {
                    format!("Contract({})", failure.error.get_code())
                } else {
                    "Abort".to_owned()
                }
            })?;
        value.to_xdr(old::Limits::none()).map_err(|e| e.to_string())
    }

    fn mock_funded_source_account(&self, label: &str, stroops: i64) -> String {
        self.host.switch_to_recording_auth(true).unwrap();
        let public_key = local_public_key(self.snapshot.network_id, label);
        let id = xdr::AccountId(xdr::PublicKey::PublicKeyTypeEd25519(xdr::Uint256(
            public_key,
        )));
        let address = xdr::ScAddress::Account(id.clone());
        let key = xdr::LedgerKey::Account(xdr::LedgerKeyAccount {
            account_id: id.clone(),
        });
        let old_key = Rc::new(historical(&key));
        let existing = self.host.get_ledger_entry(&old_key).unwrap();
        assert!(
            existing.is_none() || self.local.owns(&key),
            "generated local account collides with captured state"
        );
        self.local.mark_address(address.clone());
        self.host.set_source_account(historical(&id)).unwrap();
        let entry = funded_account_entry(
            id,
            stroops,
            self.snapshot.base_reserve,
            self.snapshot.sequence_number,
            existing.map(|(e, t)| (Rc::new(current(&*e)), t)),
        );
        self.host
            .add_ledger_entry(&old_key, &Rc::new(historical(&entry)), None)
            .unwrap();
        address.to_string()
    }

    fn replace_wasm(&self, contract: &str, wasm: &[u8]) -> [u8; 32] {
        let address: xdr::ScAddress = contract.parse().expect("invalid contract address");
        assert!(
            matches!(address, xdr::ScAddress::Contract(_)),
            "expected a contract address"
        );
        let key = Rc::new(historical(&contract_instance_key(address)));
        let (entry, ttl) = self
            .host
            .get_ledger_entry(&key)
            .unwrap()
            .expect("missing contract");
        let mut entry = (*entry).clone();
        let old::LedgerEntryData::ContractData(data) = &mut entry.data else {
            panic!("invalid contract instance");
        };
        let old::ScVal::ContractInstance(instance) = &mut data.val else {
            panic!("invalid contract instance");
        };
        assert!(
            matches!(instance.executable, old::ContractExecutable::Wasm(_)),
            "expected a WASM contract"
        );
        let hash: [u8; 32] = Sha256::digest(wasm).into();
        self.local.mark_code(hash);
        let uploaded = self
            .host
            .invoke_function(HostFunction::UploadContractWasm(wasm.try_into().unwrap()))
            .unwrap();
        assert_eq!(
            uploaded,
            old::ScVal::Bytes(hash.to_vec().try_into().unwrap())
        );
        instance.executable = old::ContractExecutable::Wasm(old::Hash(hash));
        self.host
            .add_ledger_entry(&key, &Rc::new(entry), ttl)
            .unwrap();
        hash
    }

    fn protocol_version(&self) -> u32 {
        self.host
            .with_ledger_info(|l| Ok(l.protocol_version))
            .unwrap()
    }
    fn timestamp(&self) -> u64 {
        self.host.with_ledger_info(|l| Ok(l.timestamp)).unwrap()
    }
    fn set_timestamp(&self, timestamp: u64) {
        self.host
            .with_mut_ledger_info(|l| l.timestamp = timestamp)
            .unwrap();
    }
    fn digest(&self) -> [u8; 32] {
        crate::canonical_ledger_digest(&self.state()).unwrap()
    }
    fn storage_digest(&self, contract: &str) -> [u8; 32] {
        super::storage_digest(
            self.state(),
            &contract.parse().expect("invalid contract address"),
        )
    }
}
