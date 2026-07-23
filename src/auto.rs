//! One-scenario automatic capture and strict replay.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    AccountEntry, AccountEntryExt, AccountId, LedgerEntry, LedgerEntryData, LedgerEntryExt,
    LedgerKey, LedgerKeyAccount, PublicKey, ScAddress, ScVal, SequenceNumber, String32, Thresholds,
    Uint256, VecM,
};
use soroban_sdk::{
    Address, Env, IntoVal, MuxedAddress, Symbol, TryFromVal, Val, Vec as SorobanVec,
};
use thiserror::Error;

use crate::{capture::MAINNET_PASSPHRASE, CaptureBuilder, CaptureError, CapturedFixture};

const DEFAULT_MAINNET_RPC_URL: &str = "https://mainnet.sorobanrpc.com";
const LOCAL_ACCOUNT_DOMAIN: &[u8] = b"kanatoko.local-account.v2";
const LOCAL_ACCOUNT_MIN_BALANCE: i64 = 100_000_000;
const LOCAL_ACCOUNT_BASE_RESERVES: i64 = 100;

/// Creates an automatic mainnet runner.
///
/// The same scenario closure performs dependency discovery and the final
/// strict replay. Every contract and account enters the scenario explicitly
/// through [`ScenarioFork`]; no address is privileged as a capture root.
///
/// # Panics
///
/// Panics only if Kanatoko's built-in mainnet RPC URL is not a valid HTTPS
/// origin, which would be an internal library invariant violation.
#[must_use]
pub fn mainnet() -> AutoRunner {
    let builder = CaptureBuilder::mainnet(DEFAULT_MAINNET_RPC_URL)
        .expect("the built-in mainnet RPC URL must have a valid HTTPS origin");
    AutoRunner::with_builder(builder, MAINNET_PASSPHRASE)
}

/// Runs one repeatable scenario through automatic discovery and strict replay.
pub struct AutoRunner {
    builder: CaptureBuilder,
    network_passphrase: String,
    cache: Option<PathBuf>,
    offline: bool,
    refresh: bool,
}

impl AutoRunner {
    pub(crate) fn with_builder(
        builder: CaptureBuilder,
        network_passphrase: impl Into<String>,
    ) -> Self {
        Self {
            builder,
            network_passphrase: network_passphrase.into(),
            cache: None,
            offline: false,
            refresh: false,
        }
    }

    /// Uses a scenario-specific capture bundle as a cache.
    ///
    /// A missing cache is created after a successful strict replay. A cache
    /// hit performs no RPC reads. If the cached scenario reaches an Unknown
    /// key while online, the entire scenario is recaptured from a coherent
    /// ledger and the cache is replaced atomically only after strict replay
    /// succeeds.
    #[must_use]
    pub fn cache(mut self, path: impl Into<PathBuf>) -> Self {
        self.cache = Some(path.into());
        self
    }

    /// Requires a cache hit and forbids automatic network discovery.
    #[must_use]
    pub const fn offline(mut self) -> Self {
        self.offline = true;
        self
    }

    /// Ignores an existing cache and captures a fresh coherent ledger.
    #[must_use]
    pub const fn refresh(mut self) -> Self {
        self.refresh = true;
        self
    }

    /// Runs the same closure for discovery and strict replay.
    ///
    /// Generated Soroban clients may use [`ScenarioFork::env`] while other
    /// contracts are called through [`ScenarioFork::invoke`] in the same
    /// closure and environment. A closure can execute several times and must
    /// therefore be deterministic and free of external side effects.
    ///
    /// Imported WASM used by `contractimport!` supplies only the generated
    /// client ABI. Calls to captured addresses always execute the contract
    /// instance and WASM loaded from network state.
    ///
    /// # Errors
    ///
    /// Returns a capture, cache validation, offline-cache, or strict replay
    /// error. Scenario panics remain opaque at this boundary.
    pub fn run<F>(&self, scenario: F) -> Result<AutoRun, AutoRunError>
    where
        F: for<'a> Fn(&ScenarioFork<'a>),
    {
        let cache_existed = self.cache.as_deref().is_some_and(Path::exists);
        if let Some(path) = self.cache.as_deref().filter(|_| !self.refresh) {
            if path.exists() {
                let cached = CapturedFixture::from_file(path, &self.network_passphrase)?;
                match replay(&cached, &scenario) {
                    Ok(()) => {
                        return Ok(AutoRun {
                            fixture: cached,
                            cache_status: CacheStatus::Hit,
                        });
                    }
                    Err(CaptureError::UnknownLedgerKeys { .. }) if !self.offline => {}
                    Err(error) => return Err(error.into()),
                }
            } else if self.offline {
                return Err(AutoRunError::OfflineCacheMissing {
                    path: path.to_path_buf(),
                });
            }
        }

        if self.offline {
            return Err(AutoRunError::OfflineCacheMissing {
                path: self.cache.clone().unwrap_or_default(),
            });
        }

        let captured = self.builder.capture(|env| {
            scenario(&ScenarioFork::new(env));
        })?;
        replay(&captured, &scenario)?;

        let cache_status = match self.cache.as_deref() {
            Some(path) => {
                if let Some(parent) = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    fs::create_dir_all(parent).map_err(|source| CaptureError::CaptureBundleIo {
                        operation: "create-cache-directory",
                        source,
                    })?;
                }
                captured.write_file(path)?;
                if cache_existed {
                    CacheStatus::Refreshed
                } else {
                    CacheStatus::Created
                }
            }
            None => CacheStatus::Disabled,
        };
        Ok(AutoRun {
            fixture: captured,
            cache_status,
        })
    }
}

fn replay<F>(fixture: &CapturedFixture, scenario: &F) -> Result<(), CaptureError>
where
    F: for<'a> Fn(&ScenarioFork<'a>),
{
    fixture.replay(|env| {
        scenario(&ScenarioFork::new(env));
    })
}

/// One stateful scenario pass.
///
/// The environment is never replaced during a pass, so generated clients and
/// dynamic invocations can safely share it. The runner recreates the complete
/// closure, environment, addresses, and clients for every discovery retry and
/// for final strict replay.
pub struct ScenarioFork<'a> {
    env: &'a Env,
    local_accounts: RefCell<BTreeSet<[u8; 32]>>,
}

impl<'a> ScenarioFork<'a> {
    fn new(env: &'a Env) -> Self {
        Self {
            env,
            local_accounts: RefCell::new(BTreeSet::new()),
        }
    }

    /// Current pass environment for generated Soroban clients.
    #[must_use]
    pub const fn env(&self) -> &'a Env {
        self.env
    }

    /// Parses a contract C-address in the current pass environment.
    ///
    /// # Panics
    ///
    /// Panics if `contract` is not a valid contract C-address.
    #[must_use]
    pub fn contract(&self, contract: &str) -> Address {
        let address = Address::from_str(self.env, contract);
        assert!(
            matches!(ScAddress::from(&address), ScAddress::Contract(_)),
            "expected a contract C-address"
        );
        address
    }

    /// Parses an existing Stellar G-account in the current pass environment.
    ///
    /// Ledger state is not injected or altered. Contract calls that read this
    /// address discover its real `AccountEntry`, trustlines, and other
    /// Host-supported ledger entries on demand.
    ///
    /// # Panics
    ///
    /// Panics if `account` is not a valid account G-address.
    #[must_use]
    pub fn account(&self, account: &str) -> Address {
        let address = Address::from_str(self.env, account);
        assert!(
            matches!(ScAddress::from(&address), ScAddress::Account(_)),
            "expected an account G-address"
        );
        address
    }

    /// Parses a multiplexed Stellar M-address in the current pass environment.
    ///
    /// The multiplexing ID is contract input/event metadata. When a contract
    /// resolves [`MuxedAddress::address`], ledger access targets the underlying
    /// G-account and is captured normally.
    ///
    /// # Panics
    ///
    /// Panics if `account` is not a valid multiplexed M-address.
    #[must_use]
    pub fn muxed_account(&self, account: &str) -> MuxedAddress {
        let address = MuxedAddress::from_str(self.env, account);
        assert!(
            matches!(
                ScVal::from(&address),
                ScVal::Address(ScAddress::MuxedAccount(_))
            ),
            "expected a multiplexed M-address"
        );
        address
    }

    /// Creates a funded local Stellar account and returns its G-address.
    ///
    /// The address is pseudorandom but deterministic for the captured network
    /// and `label`. This keeps dependency discovery, strict replay, cache hits,
    /// and CI runs reproducible. Reusing a label in one scenario pass returns
    /// the same account without resetting its state.
    ///
    /// This is explicit local ledger injection, not a mainnet account creation
    /// or a transaction-faithful operation. The account has no signing key;
    /// use an explicit authorization mode such as [`ScenarioFork::mock_all_auths`].
    /// Classic assets still require calling the SAC's `trust` method, and may
    /// require `set_authorized`, before minting or transferring the asset.
    ///
    /// # Panics
    ///
    /// Panics if the generated address already exists in captured network
    /// state or if the Host rejects the local account entry.
    #[must_use]
    pub fn local_account(&self, label: &str) -> Address {
        let (network_id, base_reserve, ledger_sequence) = self
            .env
            .host()
            .with_ledger_info(|ledger| {
                Ok((
                    ledger.network_id,
                    ledger.base_reserve,
                    ledger.sequence_number,
                ))
            })
            .expect("the scenario environment must have ledger metadata");
        let mut digest = Sha256::new();
        digest.update(LOCAL_ACCOUNT_DOMAIN);
        digest.update(network_id);
        digest.update((label.len() as u64).to_be_bytes());
        digest.update(label.as_bytes());
        let public_key: [u8; 32] = digest.finalize().into();
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(public_key)));
        let address = Address::try_from_val(self.env, &ScAddress::Account(account_id.clone()))
            .expect("a generated Ed25519 account must be a valid Soroban address");

        if !self.local_accounts.borrow_mut().insert(public_key) {
            return address;
        }

        let key = Rc::new(LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        }));
        if self
            .env
            .host()
            .get_ledger_entry(&key)
            .expect("the Host must be able to inspect the generated account")
            .is_some()
        {
            panic!("generated local account collides with captured network state");
        }

        let balance = i64::from(base_reserve)
            .saturating_mul(LOCAL_ACCOUNT_BASE_RESERVES)
            .max(LOCAL_ACCOUNT_MIN_BALANCE);
        let sequence = i64::from(ledger_sequence)
            .checked_mul(1_i64 << 32)
            .expect("the ledger sequence must fit a Stellar account sequence number");
        let entry = Rc::new(LedgerEntry {
            last_modified_ledger_seq: ledger_sequence,
            data: LedgerEntryData::Account(AccountEntry {
                account_id,
                balance,
                seq_num: SequenceNumber(sequence),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        });
        self.env
            .host()
            .add_ledger_entry(&key, &entry, None)
            .expect("the Host must accept a generated local account");

        address
    }

    /// Enables the SDK's explicit record-and-mock authorization mode.
    ///
    /// This is mocked behavioral evidence, not signature evidence.
    pub fn mock_all_auths(&self) {
        self.env.mock_all_auths();
    }

    /// Dynamically invokes a contract while sharing state with generated
    /// clients in this scenario pass.
    ///
    /// Heterogeneous tuples with up to 13 values are accepted as arguments,
    /// using the same SDK conversions as generated clients. The caller selects
    /// the return type with an annotation or turbofish.
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`Env::invoke_contract`], including
    /// an ABI mismatch, contract failure, or incompatible requested result
    /// type.
    #[allow(clippy::needless_pass_by_value)]
    pub fn invoke<R>(
        &self,
        contract: &Address,
        function: &str,
        args: impl IntoVal<Env, SorobanVec<Val>>,
    ) -> R
    where
        R: TryFromVal<Env, Val>,
    {
        self.env.invoke_contract(
            contract,
            &Symbol::new(self.env, function),
            args.into_val(self.env),
        )
    }
}

/// How the automatic runner obtained its fixture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheStatus {
    Disabled,
    Hit,
    Created,
    Refreshed,
}

/// Result of a successful automatic scenario.
pub struct AutoRun {
    fixture: CapturedFixture,
    cache_status: CacheStatus,
}

impl AutoRun {
    /// Captured fixture that passed the strict scenario replay.
    #[must_use]
    pub const fn fixture(&self) -> &CapturedFixture {
        &self.fixture
    }

    #[must_use]
    pub const fn cache_status(&self) -> CacheStatus {
        self.cache_status
    }
}

/// Automatic runner failures.
#[derive(Debug, Error)]
pub enum AutoRunError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error("offline mode requires an existing capture cache at {path}")]
    OfflineCacheMissing { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use soroban_sdk::testutils::EnvTestConfig;

    use super::*;

    #[test]
    fn local_account_is_derived_from_v2_domain_network_and_label_without_a_contract() {
        let mut env = Env::default();
        env.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        let network_id = env
            .host()
            .with_ledger_info(|ledger| Ok(ledger.network_id))
            .unwrap();
        let fork = ScenarioFork::new(&env);

        let actual = fork.local_account("alice");
        let mut digest = Sha256::new();
        digest.update(b"kanatoko.local-account.v2");
        digest.update(network_id);
        digest.update((5_u64).to_be_bytes());
        digest.update(b"alice");
        let expected = ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            digest.finalize().into(),
        ))));

        assert_eq!(ScAddress::from(&actual), expected);
        assert_eq!(fork.local_account("alice"), actual);
        assert_ne!(fork.local_account("bob"), actual);
    }
}
