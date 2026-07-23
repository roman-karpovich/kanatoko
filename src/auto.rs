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
    testutils::Address as _, Address, ConstructorArgs, Env, IntoVal, MuxedAddress, Symbol,
    TryFromVal, Val, Vec as SorobanVec,
};
use thiserror::Error;

use crate::{
    capture::{contract_instance_key, LocalLedger, MAINNET_PASSPHRASE},
    CaptureBuilder, CaptureError, CapturedFixture,
};

const DEFAULT_MAINNET_RPC_URL: &str = "https://mainnet.sorobanrpc.com";
const LOCAL_ACCOUNT_DOMAIN: &[u8] = b"kanatoko.local-account.v2";

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
    /// closure and environment. A cache hit executes the closure once. A cold
    /// capture can execute it several times, each in a fresh environment, so
    /// contract mutations never accumulate between discovery passes. External
    /// effects such as randomness, file or network I/O, counters, and output
    /// would be repeated; produce one-time ordinary Rust inputs before calling
    /// this method. Environment-bound values and clients must still be created
    /// inside the closure.
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

        let captured = self.builder.capture_with_local(|env, local| {
            scenario(&ScenarioFork::new(env, local));
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
    fixture.replay_with_local(|env, local| {
        scenario(&ScenarioFork::new(env, local));
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
    local_ledger: Rc<LocalLedger>,
}

impl<'a> ScenarioFork<'a> {
    fn new(env: &'a Env, local_ledger: Rc<LocalLedger>) -> Self {
        Self {
            env,
            local_accounts: RefCell::new(BTreeSet::new()),
            local_ledger,
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

    /// Creates an unfunded local Stellar address.
    ///
    /// The address is pseudorandom but deterministic for the captured network
    /// and `label`. This keeps dependency discovery, strict replay, cache hits,
    /// and CI runs reproducible. Reusing a label in one scenario pass returns
    /// the same account without resetting its state.
    ///
    /// No `AccountEntry`, XLM, trustline, or signing key is created implicitly.
    /// Fund it explicitly with [`ScenarioFork::fund_local_account`] when the
    /// scenario needs an existing account. Use an explicit authorization mode
    /// such as [`ScenarioFork::mock_all_auths`]. Classic assets still require
    /// calling the SAC's `trust` method, and may require `set_authorized`,
    /// before minting or transferring the asset.
    ///
    /// # Panics
    ///
    /// Panics if the generated address already exists in captured network
    /// state.
    #[must_use]
    pub fn local_account(&self, label: &str) -> Address {
        let network_id = self
            .env
            .host()
            .with_ledger_info(|ledger| Ok(ledger.network_id))
            .expect("the scenario environment must have ledger metadata");
        let mut digest = Sha256::new();
        digest.update(LOCAL_ACCOUNT_DOMAIN);
        digest.update(network_id);
        digest.update((label.len() as u64).to_be_bytes());
        digest.update(label.as_bytes());
        let public_key: [u8; 32] = digest.finalize().into();
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(public_key)));
        let address_xdr = ScAddress::Account(account_id.clone());
        let address = Address::try_from_val(self.env, &address_xdr)
            .expect("a generated Ed25519 account must be a valid Soroban address");

        if self.local_accounts.borrow().contains(&public_key) {
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
        let inserted = self.local_accounts.borrow_mut().insert(public_key);
        assert!(inserted, "local account tracking must be deterministic");
        self.local_ledger.mark_address(address_xdr);

        address
    }

    /// Adds `amount` stroops to an address created by [`Self::local_account`].
    ///
    /// The first funding creates a valid `AccountEntry` with no subentries; its
    /// amount must cover the network's two-base-reserve minimum. Later calls
    /// add to the existing balance. This is explicit local ledger injection,
    /// not a Stellar payment or transaction-faithful funding operation.
    ///
    /// # Panics
    ///
    /// Panics if `amount` is not positive, first funding is below the minimum,
    /// `account` was not created by this scenario pass, its existing entry is
    /// malformed, the resulting balance overflows `i64`, or the Host rejects
    /// the entry.
    pub fn fund_local_account(&self, account: &Address, amount: i64) {
        assert!(amount > 0, "local account funding must be positive");
        let ScAddress::Account(account_id) = ScAddress::from(account) else {
            panic!("expected a local G-account");
        };
        let AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(public_key))) = &account_id;
        assert!(
            self.local_accounts.borrow().contains(public_key),
            "account was not created by this scenario pass"
        );

        let key = Rc::new(LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        }));
        let existing = self
            .env
            .host()
            .get_ledger_entry(&key)
            .expect("the Host must be able to inspect the local account");
        let (base_reserve, ledger_sequence) = self
            .env
            .host()
            .with_ledger_info(|ledger| Ok((ledger.base_reserve, ledger.sequence_number)))
            .expect("the scenario environment must have ledger metadata");
        let entry = if let Some((entry, live_until)) = existing {
            assert!(
                live_until.is_none(),
                "local AccountEntry must not have a live-until ledger"
            );
            let mut entry = (*entry).clone();
            let LedgerEntryData::Account(local) = &mut entry.data else {
                panic!("local account key must contain an AccountEntry");
            };
            assert_eq!(
                local.account_id, account_id,
                "local AccountEntry must match its ledger key"
            );
            local.balance = local
                .balance
                .checked_add(amount)
                .expect("local account balance overflow");
            entry.last_modified_ledger_seq = ledger_sequence;
            entry
        } else {
            let minimum = i64::from(base_reserve)
                .checked_mul(2)
                .expect("local account minimum balance overflow");
            assert!(
                amount >= minimum,
                "first local account funding must cover two base reserves"
            );
            let sequence = i64::from(ledger_sequence)
                .checked_mul(1_i64 << 32)
                .expect("the ledger sequence must fit a Stellar account sequence number");
            LedgerEntry {
                last_modified_ledger_seq: ledger_sequence,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id,
                    balance: amount,
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
            }
        };
        self.env
            .host()
            .add_ledger_entry(&key, &Rc::new(entry), None)
            .expect("the Host must accept explicit local account funding");
    }

    /// Locally installs candidate WASM and runs its constructor, if defined.
    ///
    /// The returned contract shares this scenario's mutable environment, so it
    /// can call captured network contracts and mutate their forked state.
    /// Installation is deterministic when the scenario is deterministic. Its
    /// code and contract-owned storage remain local rather than becoming
    /// network-cache dependencies; the generated contract address is still
    /// checked against captured network state before installation.
    ///
    /// This uses the SDK test registration path. It does not emulate Soroban
    /// upload/create transactions, deployment authorization, fees, signatures,
    /// or Stellar Core consensus.
    ///
    /// # Panics
    ///
    /// Panics if the generated address collides with captured network state,
    /// the WASM is invalid, its constructor fails, or the Host rejects local
    /// installation.
    #[must_use]
    pub fn deploy<A>(&self, wasm: &[u8], constructor_args: A) -> Address
    where
        A: ConstructorArgs,
    {
        let address = Address::generate(self.env);
        let address_xdr = ScAddress::from(&address);
        assert!(
            matches!(address_xdr, ScAddress::Contract(_)),
            "generated candidate address must be a contract"
        );
        let key = Rc::new(contract_instance_key(address_xdr.clone()));
        if self
            .env
            .host()
            .get_ledger_entry(&key)
            .expect("the Host must be able to inspect the candidate address")
            .is_some()
        {
            panic!("generated candidate address collides with captured network state");
        }

        self.local_ledger.mark_address(address_xdr);
        self.local_ledger.mark_code(Sha256::digest(wasm).into());
        self.env.register_at(&address, wasm, constructor_args)
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
    use soroban_sdk::testutils::{EnvTestConfig, Ledger as _};

    use super::*;

    mod stateful {
        soroban_sdk::contractimport!(
            file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
            sha256 = "6f6f469798b686cc485ad207f32e3f77009c4b69ab2437d9bdca97f149b54ba8",
        );
    }

    #[test]
    fn local_account_is_unfunded_and_explicit_funding_is_scoped_and_additive() {
        let mut env = Env::default();
        env.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        env.ledger().set_base_reserve(5_000_000);
        env.ledger().set_sequence_number(123);
        let network_id = env
            .host()
            .with_ledger_info(|ledger| Ok(ledger.network_id))
            .unwrap();
        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()));

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
        assert_eq!(local_account_balance(&env, &actual), None);
        assert_eq!(fork.local_account("alice"), actual);
        assert_ne!(fork.local_account("bob"), actual);

        let minimum = env
            .host()
            .with_ledger_info(|ledger| Ok(i64::from(ledger.base_reserve) * 2))
            .unwrap();
        fork.fund_local_account(&actual, minimum);
        assert_eq!(local_account_balance(&env, &actual), Some(minimum));
        fork.fund_local_account(&actual, 5);
        assert_eq!(local_account_balance(&env, &actual), Some(minimum + 5));
        assert_eq!(fork.local_account("alice"), actual);
        assert_eq!(local_account_balance(&env, &actual), Some(minimum + 5));

        let below_reserve = fork.local_account("below-reserve");
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fork.fund_local_account(&below_reserve, minimum - 1);
        }))
        .is_err());
        assert_eq!(local_account_balance(&env, &below_reserve), None);

        let nonlocal = Address::from_str(
            &env,
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
        );
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fork.fund_local_account(&nonlocal, minimum);
        }))
        .is_err());
    }

    fn local_account_balance(env: &Env, account: &Address) -> Option<i64> {
        let ScAddress::Account(account_id) = ScAddress::from(account) else {
            panic!("local account must be a G-address");
        };
        let key = Rc::new(LedgerKey::Account(LedgerKeyAccount { account_id }));
        let (entry, _) = env.host().get_ledger_entry(&key).unwrap()?;
        let LedgerEntryData::Account(account) = &entry.data else {
            panic!("local account key must contain an AccountEntry");
        };
        Some(account.balance)
    }

    #[test]
    fn deploy_rejects_generated_address_collision_without_replacing_contract() {
        let mut env = Env::default();
        env.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        let mut mirror = Env::default();
        mirror.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        let occupied_xdr = ScAddress::from(&Address::generate(&mirror));
        let occupied = Address::try_from_val(&env, &occupied_xdr).unwrap();
        env.register_at(&occupied, stateful::WASM, (41_i64,));
        let existing = stateful::Client::new(&env, &occupied);
        assert_eq!(existing.get(), 41);

        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fork.deploy(stateful::WASM, (99_i64,));
        }))
        .is_err());
        assert_eq!(existing.get(), 41);
    }
}
