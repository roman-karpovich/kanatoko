//! One-scenario automatic capture and strict replay.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    panic::{catch_unwind, AssertUnwindSafe, Location},
    path::{Path, PathBuf},
    rc::Rc,
};

use sha2::{Digest, Sha256};
use soroban_env_host::{
    xdr::{
        AccountEntry, AccountEntryExt, AccountId, ContractExecutable, Hash, LedgerEntry,
        LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyAccount, PublicKey, ScAddress,
        ScSymbol, ScVal, SequenceNumber, String32, Thresholds, Uint256, VecM,
    },
    InvocationResources,
};
use soroban_sdk::{
    testutils::Address as _, Address, ConstructorArgs, Env, IntoVal, MuxedAddress, Symbol,
    TryFromVal, Val, Vec as SorobanVec,
};
use thiserror::Error;

use crate::{
    capture::{
        contract_instance_key, LocalLedger, TrackingSource, MAINNET_PASSPHRASE, TESTNET_PASSPHRASE,
    },
    runtime::configure_fork_env,
    strict::{
        detached_diagnostics, detached_snapshot_evidence, invocation_values, invoke_once,
        invoke_outcome, is_nonce_data, state_diff, strip_new_mock_nonces,
    },
    AppliedAuthMode, AuthorizationTree, CaptureBuilder, CaptureError, CapturedFixture,
    InvocationFailure, InvokeErrorKind, InvokeOutcome, InvokeRequest, Receipt, ReceiptDisposition,
    StateChange, StrictForkError,
};

const DEFAULT_MAINNET_RPC_URL: &str = "https://mainnet.sorobanrpc.com";
const DEFAULT_TESTNET_RPC_URL: &str = "https://soroban-testnet.stellar.org";
const LOCAL_ACCOUNT_DOMAIN: &[u8] = b"kanatoko.local-account.v2";
const DEFAULT_CACHE_DOMAIN: &[u8] = b"kanatoko.default-cache.v2";
const DEFAULT_CACHE_NAME_MAX: usize = 80;
const DEFAULT_CACHE_HASH_HEX: usize = 12;

/// Creates an automatic mainnet runner using
/// `https://mainnet.sorobanrpc.com` by default.
///
/// The same scenario closure performs dependency discovery and the final
/// strict replay. Every contract and account enters the scenario explicitly
/// through [`ScenarioFork`]; no address is privileged as a capture root.
/// A default cache path under `.kanatoko/` is derived from the current thread
/// name and this callsite. [`AutoRunner::cache`] remains an explicit override.
///
#[must_use]
#[track_caller]
pub fn mainnet() -> AutoRunner {
    network_runner(
        DEFAULT_MAINNET_RPC_URL,
        MAINNET_PASSPHRASE,
        Location::caller(),
    )
}

/// Creates an automatic public testnet runner using
/// `https://soroban-testnet.stellar.org` by default.
///
/// The runner has the same capture, cache, and strict replay semantics as
/// [`mainnet`], but fixes network identity to Stellar public testnet. Use
/// [`AutoRunner::rpc_url`] to select another testnet RPC provider.
#[must_use]
#[track_caller]
pub fn testnet() -> AutoRunner {
    network_runner(
        DEFAULT_TESTNET_RPC_URL,
        TESTNET_PASSPHRASE,
        Location::caller(),
    )
}

fn network_runner(
    rpc_url: &str,
    network_passphrase: &str,
    caller: &'static Location<'static>,
) -> AutoRunner {
    let thread = std::thread::current();
    let cache = default_cache_path(
        network_passphrase,
        thread.name().unwrap_or("scenario"),
        caller.file(),
        caller.line(),
        caller.column(),
    );
    AutoRunner::with_rpc_url(rpc_url, network_passphrase).cache(cache)
}

fn default_cache_path(
    network_passphrase: &str,
    thread_name: &str,
    caller_file: &str,
    caller_line: u32,
    caller_column: u32,
) -> PathBuf {
    let human = sanitize_cache_name(thread_name);
    let mut digest = Sha256::new();
    digest.update(DEFAULT_CACHE_DOMAIN);
    digest.update((network_passphrase.len() as u64).to_be_bytes());
    digest.update(network_passphrase.as_bytes());
    digest.update((thread_name.len() as u64).to_be_bytes());
    digest.update(thread_name.as_bytes());
    digest.update((caller_file.len() as u64).to_be_bytes());
    digest.update(caller_file.as_bytes());
    digest.update(caller_line.to_be_bytes());
    digest.update(caller_column.to_be_bytes());
    let digest: [u8; 32] = digest.finalize().into();
    let mut short_hash = String::with_capacity(DEFAULT_CACHE_HASH_HEX);
    for byte in &digest[..DEFAULT_CACHE_HASH_HEX / 2] {
        write!(&mut short_hash, "{byte:02x}").expect("String writes are infallible");
    }
    PathBuf::from(".kanatoko").join(format!("{human}-{short_hash}.json"))
}

fn sanitize_cache_name(raw: &str) -> String {
    let mut sanitized = String::with_capacity(raw.len().min(DEFAULT_CACHE_NAME_MAX));
    let mut separator_pending = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if separator_pending
                && !sanitized.is_empty()
                && sanitized.len() < DEFAULT_CACHE_NAME_MAX
            {
                sanitized.push('-');
            }
            separator_pending = false;
            if sanitized.len() == DEFAULT_CACHE_NAME_MAX {
                break;
            }
            sanitized.push(ch);
        } else {
            separator_pending = true;
        }
    }
    while sanitized.ends_with('-') {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        "scenario".to_string()
    } else {
        sanitized
    }
}

/// Runs one repeatable scenario through automatic discovery and strict replay.
pub struct AutoRunner {
    source: CaptureSource,
    network_passphrase: String,
    cache: Option<PathBuf>,
    offline: bool,
    refresh: bool,
}

enum CaptureSource {
    Http {
        rpc_url: String,
    },
    #[cfg(test)]
    Builder(CaptureBuilder),
}

impl AutoRunner {
    fn with_rpc_url(rpc_url: impl Into<String>, network_passphrase: impl Into<String>) -> Self {
        Self {
            source: CaptureSource::Http {
                rpc_url: rpc_url.into(),
            },
            network_passphrase: network_passphrase.into(),
            cache: None,
            offline: false,
            refresh: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_builder(
        builder: CaptureBuilder,
        network_passphrase: impl Into<String>,
    ) -> Self {
        Self {
            source: CaptureSource::Builder(builder),
            network_passphrase: network_passphrase.into(),
            cache: None,
            offline: false,
            refresh: false,
        }
    }

    /// Uses another RPC provider for the runner's selected network.
    ///
    /// This changes only the capture provider. The network passphrase, strict
    /// cache validation, and automatic cache identity remain unchanged. URL
    /// validation is deferred until capture is required, so a cache hit or
    /// offline replay never validates or contacts this provider.
    #[must_use]
    pub fn rpc_url(mut self, url: impl Into<String>) -> Self {
        self.source = CaptureSource::Http {
            rpc_url: url.into(),
        };
        self
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

        let captured = match &self.source {
            CaptureSource::Http { rpc_url } => {
                let builder =
                    CaptureBuilder::rpc(rpc_url.clone(), self.network_passphrase.clone())?;
                builder.capture_with_local(|env, local, source| {
                    scenario(&ScenarioFork::new(env, local, Some(source)));
                })?
            }
            #[cfg(test)]
            CaptureSource::Builder(builder) => {
                builder.capture_with_local(|env, local, source| {
                    scenario(&ScenarioFork::new(env, local, Some(source)));
                })?
            }
        };
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
    fixture.replay_with_local(|env, local, source| {
        scenario(&ScenarioFork::new(env, local, Some(source)));
    })
}

/// Authorization policy for an isolated [`ScenarioFork::preview`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewAuth {
    /// Record and mock-satisfy the authorization tree requested by the call.
    Record,
    /// Record and require an exact detached authorization tree.
    ///
    /// Exact validation is intentionally preview-only. It does not apply state
    /// to the outer scenario environment.
    Exact(Vec<AuthorizationTree>),
}

/// Detached evidence from one isolated high-level preview.
///
/// Resources are the local Host estimate for this invocation. They do not
/// model transaction-envelope work and are not a network fee quote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationReport {
    receipt: Receipt,
    resources: Option<InvocationResources>,
}

impl InvocationReport {
    #[must_use]
    pub const fn receipt(&self) -> &Receipt {
        &self.receipt
    }

    #[must_use]
    pub const fn resources(&self) -> Option<&InvocationResources> {
        self.resources.as_ref()
    }

    #[must_use]
    pub fn authorization(&self) -> &[AuthorizationTree] {
        &self.receipt.authorization
    }

    #[must_use]
    pub fn events(&self) -> &[crate::DetachedEvent] {
        &self.receipt.events
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[crate::DetachedEvent] {
        &self.receipt.diagnostics
    }

    #[must_use]
    pub fn state_changes(&self) -> &[StateChange] {
        &self.receipt.state_changes
    }

    /// Converts the detached return value into an SDK value in `env`.
    ///
    /// A conversion failure is reported as
    /// [`InvokeErrorKind::ResultConversion`]; this method never executes the
    /// contract again.
    ///
    /// # Errors
    ///
    /// Returns the invocation's detached failure, or a result-conversion
    /// failure when `R` is incompatible with the returned [`ScVal`].
    pub fn result<R>(&self, env: &Env) -> Result<R, InvocationFailure>
    where
        R: TryFromVal<Env, Val>,
    {
        let value = match &self.receipt.outcome {
            InvokeOutcome::Success(value) => value,
            InvokeOutcome::Failure { error, kind } => {
                return Err(InvocationFailure {
                    error: error.clone(),
                    kind: *kind,
                });
            }
        };
        let value = Val::try_from_val(env, value).map_err(|_| InvocationFailure {
            error: None,
            kind: InvokeErrorKind::ResultConversion,
        })?;
        R::try_from_val(env, &value).map_err(|_| InvocationFailure {
            error: None,
            kind: InvokeErrorKind::ResultConversion,
        })
    }
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
    tracking_source: Option<Rc<TrackingSource>>,
}

impl<'a> ScenarioFork<'a> {
    fn new(
        env: &'a Env,
        local_ledger: Rc<LocalLedger>,
        tracking_source: Option<Rc<TrackingSource>>,
    ) -> Self {
        Self {
            env,
            local_accounts: RefCell::new(BTreeSet::new()),
            local_ledger,
            tracking_source,
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
        assert!(address.id().is_some(), "expected a multiplexed M-address");
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

    /// Replaces an existing captured contract's WASM without changing its
    /// address or storage.
    ///
    /// The replacement code is installed locally, then only the executable
    /// hash in the existing contract instance is changed. Instance,
    /// persistent, and temporary storage remain intact, the instance TTL is
    /// preserved, and the replacement constructor is not called. Calls from
    /// other captured contracts to the same address therefore execute the
    /// replacement code.
    ///
    /// This is a test-only code override. It does not invoke the production
    /// contract's upgrade entrypoint or emulate upgrade authorization,
    /// transaction envelopes, fees, signatures, or consensus.
    ///
    /// Returns the SHA-256 hash of the replacement WASM.
    ///
    /// # Panics
    ///
    /// Panics if `contract` is not an existing WASM contract, `wasm` is
    /// invalid, or the Host rejects the local code override.
    pub fn replace_wasm(&self, contract: &Address, wasm: &[u8]) -> [u8; 32] {
        let address = ScAddress::from(contract);
        assert!(
            matches!(address, ScAddress::Contract(_)),
            "WASM can only replace a contract C-address"
        );

        let instance_key = Rc::new(contract_instance_key(address));
        let (instance_entry, live_until_ledger) = self
            .env
            .host()
            .get_ledger_entry(&instance_key)
            .expect("the Host must be able to inspect the existing contract")
            .expect("WASM can only replace an existing contract");
        let mut instance_entry = instance_entry.as_ref().clone();
        let LedgerEntryData::ContractData(instance_data) = &mut instance_entry.data else {
            panic!("contract instance key must contain ContractData");
        };
        let ScVal::ContractInstance(instance) = &mut instance_data.val else {
            panic!("contract instance key must contain a contract instance");
        };
        assert!(
            matches!(instance.executable, ContractExecutable::Wasm(_)),
            "WASM cannot replace a Stellar Asset Contract"
        );

        let wasm_sha256: [u8; 32] = Sha256::digest(wasm).into();
        self.local_ledger.mark_code(wasm_sha256);
        let uploaded = self.env.deployer().upload_contract_wasm(wasm);
        assert_eq!(
            uploaded.to_array(),
            wasm_sha256,
            "the Host returned an unexpected replacement WASM hash"
        );

        instance.executable = ContractExecutable::Wasm(Hash(wasm_sha256));
        self.env
            .host()
            .add_ledger_entry(&instance_key, &Rc::new(instance_entry), live_until_ledger)
            .expect("the Host must accept the local code override");

        wasm_sha256
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

    /// Dynamically invokes a contract once in the current mutable environment.
    ///
    /// Contract and Host failures are returned as structured values without
    /// parsing panic or diagnostic strings. Failed Host invocations roll back
    /// their contract state atomically. As with [`Env::try_invoke_contract`], a
    /// [`InvokeErrorKind::ResultConversion`] is detected only after a successful
    /// Host call, so the call's state may already have been applied.
    ///
    /// # Errors
    ///
    /// Returns a structured contract, Host, panic, or return-conversion
    /// failure. No diagnostic or panic strings are parsed.
    #[allow(clippy::needless_pass_by_value)]
    pub fn try_invoke<R>(
        &self,
        contract: &Address,
        function: &str,
        args: impl IntoVal<Env, SorobanVec<Val>>,
    ) -> Result<R, InvocationFailure>
    where
        R: TryFromVal<Env, Val>,
    {
        let prepared = catch_unwind(AssertUnwindSafe(|| {
            let function = function
                .try_into()
                .map(ScSymbol)
                .map_err(|_| InvocationFailure {
                    error: None,
                    kind: InvokeErrorKind::Abort,
                })?;
            let function =
                Symbol::try_from_val(self.env, &ScVal::Symbol(function)).map_err(|_| {
                    InvocationFailure {
                        error: None,
                        kind: InvokeErrorKind::Abort,
                    }
                })?;
            Ok::<_, InvocationFailure>((function, args.into_val(self.env)))
        }));
        let (function, args) = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(failure)) => return Err(failure),
            Err(_) => {
                return Err(InvocationFailure {
                    error: None,
                    kind: InvokeErrorKind::Abort,
                });
            }
        };
        invoke_once(self.env, contract, &function, args)
    }

    /// Executes one contract call in an isolated child environment.
    ///
    /// The returned report contains detached return/error, authorization,
    /// events, diagnostics, state changes, and an optional local resource
    /// estimate. The outer environment's state, events, authorization, and SDK
    /// generators are never changed. Locally deployed WASM and captured
    /// cross-contract calls remain available through the cloned snapshot.
    ///
    /// Preview resources are local Host estimates, not network fee parity.
    ///
    /// # Errors
    ///
    /// Returns a typed strict error for invalid detached values, exact-auth
    /// mismatch, Host inspection failure, or an unknown offline dependency.
    #[allow(clippy::needless_pass_by_value)]
    pub fn preview(
        &self,
        contract: &Address,
        function: &str,
        args: impl IntoVal<Env, SorobanVec<Val>>,
        auth: PreviewAuth,
    ) -> Result<InvocationReport, StrictForkError> {
        let function = ScSymbol(
            function
                .try_into()
                .map_err(|_| StrictForkError::InvalidInvocationXdr)?,
        );
        let args = catch_unwind(AssertUnwindSafe(|| {
            let args = args.into_val(self.env);
            args.iter()
                .map(|arg| {
                    ScVal::try_from_val(self.env, &arg)
                        .map_err(|_| StrictForkError::InvalidInvocationXdr)
                })
                .collect::<Result<Vec<_>, _>>()
        }))
        .map_err(|_| StrictForkError::InvalidInvocationXdr)??;
        let request = InvokeRequest {
            contract: ScAddress::from(contract),
            function,
            args,
        };

        let before = self.env.to_ledger_snapshot();
        let before_digest = crate::canonical_ledger_digest(&before)?;
        let mut child = Env::from_snapshot(self.env.to_snapshot());
        configure_fork_env(&mut child);
        child.mock_all_auths();
        let (contract, function, args) = invocation_values(&child, &request)?;
        let outcome = invoke_outcome(&child, &contract, &function, args);
        let upstream_reads = self.forward_preview_reads(&child)?;
        let resources = child.host().get_last_invocation_resources();
        let (authorization, events) = detached_snapshot_evidence(&child);
        let (auth_mode, expected) = match auth {
            PreviewAuth::Record => (AppliedAuthMode::RecordMockSatisfied, None),
            PreviewAuth::Exact(expected) => (AppliedAuthMode::MockExact, Some(expected)),
        };
        if expected
            .as_ref()
            .is_some_and(|expected| expected != &authorization)
        {
            return Err(StrictForkError::AuthTreeMismatch);
        }

        let mut after = child.to_ledger_snapshot();
        strip_new_mock_nonces(&before, &mut after)?;
        let after_digest = crate::canonical_ledger_digest(&after)?;
        let state_changes = state_diff(&before, &after)?;
        let diagnostics = detached_diagnostics(&child)?;
        Ok(InvocationReport {
            receipt: Receipt {
                request,
                outcome,
                auth_mode,
                authorization,
                events,
                diagnostics,
                state_changes,
                before_digest,
                after_digest,
                disposition: ReceiptDisposition::Previewed,
                upstream_reads,
            },
            resources,
        })
    }

    fn forward_preview_reads(&self, child: &Env) -> Result<u64, StrictForkError> {
        let Some(source) = &self.tracking_source else {
            return Ok(0);
        };
        let reads_before = source.rpc_reads();
        let mut inspection_failed = false;
        for (key, _) in child
            .host()
            .get_stored_entries()
            .map_err(|_| StrictForkError::HostInspection)?
        {
            if matches!(
                key.as_ref(),
                LedgerKey::ContractData(data) if is_nonce_data(data)
            ) {
                continue;
            }
            if source.observe(&key).is_err() {
                inspection_failed = true;
            }
        }
        let unknown = source.unknown_keys();
        if !unknown.is_empty() {
            return Err(StrictForkError::UnknownLedgerKeys {
                count: unknown.len(),
                keys: unknown.into_iter().collect(),
            });
        }
        if inspection_failed {
            return Err(StrictForkError::HostInspection);
        }
        Ok(source.rpc_reads().saturating_sub(reads_before))
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
    use soroban_env_host::xdr::ScError;
    use soroban_sdk::testutils::{EnvTestConfig, Ledger as _};

    use super::*;

    mod stateful {
        soroban_sdk::contractimport!(
            file = "fixtures/wasm/kanatoko_stateful_fixture.wasm",
            sha256 = "6f6f469798b686cc485ad207f32e3f77009c4b69ab2437d9bdca97f149b54ba8",
        );
    }

    mod legacy_v25 {
        soroban_sdk::contractimport!(
            file = "fixtures/wasm/kanatoko_legacy_v25_fixture.wasm",
            sha256 = "d601c7569be29b0a52af409ed65425b8c3595db8a83c444fe65dd8294423a879",
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
        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()), None);

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

        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()), None);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fork.deploy(stateful::WASM, (99_i64,));
        }))
        .is_err());
        assert_eq!(existing.get(), 41);
    }

    #[test]
    fn replace_wasm_preserves_instance_and_does_not_run_constructor() {
        let mut env = Env::default();
        env.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        let contract = env.register(stateful::WASM, (41_i64,));
        let instance_key = Rc::new(contract_instance_key((&contract).into()));
        let before = env.host().get_ledger_entry(&instance_key).unwrap().unwrap();

        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()), None);
        fork.replace_wasm(&contract, legacy_v25::WASM);
        assert_eq!(legacy_v25::Client::new(&env, &contract).sdk_major(), 25);

        assert_eq!(
            fork.replace_wasm(&contract, stateful::WASM),
            <[u8; 32]>::from(Sha256::digest(stateful::WASM))
        );

        let after = env.host().get_ledger_entry(&instance_key).unwrap().unwrap();
        assert_eq!(after, before);
        assert_eq!(stateful::Client::new(&env, &contract).get(), 41);
    }

    #[test]
    fn try_invoke_applies_success_and_returns_contract_failure_atomically() {
        let mut env = Env::default();
        env.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()), None);
        let contract = fork.deploy(stateful::WASM, (41_i64,));

        assert_eq!(
            fork.try_invoke::<i64>(&contract, "increment", (1_i64,))
                .unwrap(),
            42
        );

        let failure = fork
            .try_invoke::<i64>(&contract, "increment_then_fail", (5_i64,))
            .unwrap_err();
        assert_eq!(failure.error, Some(ScError::Contract(1)));
        assert_eq!(failure.kind, InvokeErrorKind::Contract(1));
        assert_eq!(fork.invoke::<i64>(&contract, "get", ()), 42);
    }

    #[test]
    fn try_invoke_rejects_long_function_without_panicking_or_mutating() {
        let mut env = Env::default();
        env.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()), None);
        let contract = fork.deploy(stateful::WASM, (41_i64,));
        let before = env.to_snapshot();

        let failure = fork
            .try_invoke::<i64>(
                &contract,
                "function_name_that_is_definitely_longer_than_thirty_two_characters",
                (),
            )
            .unwrap_err();

        assert_eq!(failure.error, None);
        assert_eq!(failure.kind, InvokeErrorKind::Abort);
        assert_eq!(env.to_snapshot(), before);
    }

    #[test]
    fn preview_reports_detached_evidence_without_leaking_into_outer_env() {
        let mut env = Env::default();
        env.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()), None);
        let contract = fork.deploy(stateful::WASM, (41_i64,));
        let outer_before = env.to_snapshot();

        let report = fork
            .preview(&contract, "increment", (1_i64,), PreviewAuth::Record)
            .unwrap();

        assert_eq!(report.result::<i64>(&env).unwrap(), 42);
        assert_eq!(report.receipt().disposition, ReceiptDisposition::Previewed);
        assert!(!report.events().is_empty());
        assert!(!report.state_changes().is_empty());
        assert!(report.resources().is_some());
        assert_eq!(env.to_snapshot().events, outer_before.events);
        assert_eq!(env.to_snapshot().auth, outer_before.auth);
        assert_eq!(fork.invoke::<i64>(&contract, "get", ()), 41);
    }

    #[test]
    fn preview_record_then_exact_validates_auth_without_mutation() {
        let mut env = Env::default();
        env.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()), None);
        let contract = fork.deploy(stateful::WASM, (41_i64,));
        let user = fork.local_account("preview-auth-user");
        let outer_before = env.to_snapshot();

        let recorded = fork
            .preview(
                &contract,
                "authorized_increment",
                (user.clone(), 1_i64),
                PreviewAuth::Record,
            )
            .unwrap();
        assert_eq!(recorded.result::<i64>(&env).unwrap(), 42);
        assert!(!recorded.authorization().is_empty());

        let exact = fork
            .preview(
                &contract,
                "authorized_increment",
                (user.clone(), 1_i64),
                PreviewAuth::Exact(recorded.authorization().to_vec()),
            )
            .unwrap();
        assert_eq!(exact.result::<i64>(&env).unwrap(), 42);

        let mismatch = fork
            .preview(
                &contract,
                "authorized_increment",
                (user, 1_i64),
                PreviewAuth::Exact(Vec::new()),
            )
            .unwrap_err();
        assert!(matches!(mismatch, StrictForkError::AuthTreeMismatch));
        assert_eq!(env.to_snapshot().events, outer_before.events);
        assert_eq!(env.to_snapshot().auth, outer_before.auth);
        assert_eq!(fork.invoke::<i64>(&contract, "get", ()), 41);
    }

    #[test]
    fn preview_rejects_long_function_without_panicking_or_mutating() {
        let mut env = Env::default();
        env.set_config(EnvTestConfig {
            capture_snapshot_at_drop: false,
        });
        let fork = ScenarioFork::new(&env, Rc::new(LocalLedger::default()), None);
        let contract = fork.deploy(stateful::WASM, (41_i64,));
        let before = env.to_snapshot();

        let failure = fork
            .preview(
                &contract,
                "function_name_that_is_definitely_longer_than_thirty_two_characters",
                (),
                PreviewAuth::Record,
            )
            .unwrap_err();

        assert!(matches!(failure, StrictForkError::InvalidInvocationXdr));
        assert_eq!(env.to_snapshot(), before);
    }

    #[test]
    fn default_cache_path_is_deterministic_sanitized_and_bounded() {
        let first = mainnet_from_stable_callsite();
        let second = mainnet_from_stable_callsite();
        assert_eq!(first.cache, second.cache);
        assert_ne!(first.cache, mainnet_from_other_callsite().cache);
        let path = first
            .cache
            .expect("public mainnet runner must cache by default");
        assert_eq!(path.parent(), Some(Path::new(".kanatoko")));
        let file_name = path.file_name().unwrap().to_str().unwrap();
        assert!(Path::new(file_name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json")));
        assert!(file_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')));

        let sanitized = default_cache_path(
            MAINNET_PASSPHRASE,
            "module::test / unicode 🚀 and a very long suffix that must not make filesystem paths surprising 0123456789",
            "tests/example.rs",
            12,
            34,
        );
        let stem = sanitized.file_stem().unwrap().to_str().unwrap();
        let (human, hash) = stem.rsplit_once('-').unwrap();
        assert!(human.len() <= DEFAULT_CACHE_NAME_MAX);
        assert_eq!(hash.len(), DEFAULT_CACHE_HASH_HEX);
        assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(!human.contains("--"));
        assert!(
            default_cache_path(MAINNET_PASSPHRASE, "🚀", "tests/example.rs", 12, 34)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("scenario-")
        );

        let punctuation_a = default_cache_path(
            MAINNET_PASSPHRASE,
            "module::test",
            "tests/example.rs",
            12,
            34,
        );
        let punctuation_b = default_cache_path(
            MAINNET_PASSPHRASE,
            "module--test",
            "tests/example.rs",
            12,
            34,
        );
        assert_eq!(
            sanitize_cache_name("module::test"),
            sanitize_cache_name("module--test")
        );
        assert_ne!(cache_hash(&punctuation_a), cache_hash(&punctuation_b));

        let long_prefix = "a".repeat(DEFAULT_CACHE_NAME_MAX);
        let unicode_a = default_cache_path(
            MAINNET_PASSPHRASE,
            &format!("{long_prefix}🚀first"),
            "tests/example.rs",
            12,
            34,
        );
        let unicode_b = default_cache_path(
            MAINNET_PASSPHRASE,
            &format!("{long_prefix}🎯second"),
            "tests/example.rs",
            12,
            34,
        );
        assert_eq!(
            unicode_a.file_stem().unwrap().to_str().unwrap()[..DEFAULT_CACHE_NAME_MAX],
            unicode_b.file_stem().unwrap().to_str().unwrap()[..DEFAULT_CACHE_NAME_MAX]
        );
        assert_ne!(cache_hash(&unicode_a), cache_hash(&unicode_b));

        let case_a = default_cache_path(MAINNET_PASSPHRASE, "CaseName", "tests/example.rs", 12, 34);
        let case_b = default_cache_path(MAINNET_PASSPHRASE, "casename", "tests/example.rs", 12, 34);
        assert_ne!(cache_hash(&case_a), cache_hash(&case_b));
    }

    #[test]
    fn public_network_runners_use_expected_defaults_and_network_cache_identity() {
        let mainnet_runner = mainnet_from_stable_callsite();
        let testnet_runner = testnet_from_stable_callsite();

        assert_eq!(mainnet_runner.network_passphrase, MAINNET_PASSPHRASE);
        assert_eq!(testnet_runner.network_passphrase, TESTNET_PASSPHRASE);
        assert!(matches!(
            &mainnet_runner.source,
            CaptureSource::Http { rpc_url } if rpc_url == DEFAULT_MAINNET_RPC_URL
        ));
        assert!(matches!(
            &testnet_runner.source,
            CaptureSource::Http { rpc_url } if rpc_url == DEFAULT_TESTNET_RPC_URL
        ));

        let mainnet_path = default_cache_path(
            MAINNET_PASSPHRASE,
            "module::scenario",
            "tests/example.rs",
            12,
            34,
        );
        let testnet_path = default_cache_path(
            TESTNET_PASSPHRASE,
            "module::scenario",
            "tests/example.rs",
            12,
            34,
        );
        assert_ne!(mainnet_path, testnet_path);
    }

    #[test]
    fn rpc_override_changes_provider_without_changing_network_or_default_cache() {
        let default = mainnet_from_stable_callsite();
        let overridden = mainnet_from_stable_callsite().rpc_url("https://rpc.example.test/private");

        assert_eq!(overridden.network_passphrase, MAINNET_PASSPHRASE);
        assert_eq!(overridden.cache, default.cache);
        assert!(matches!(
            &overridden.source,
            CaptureSource::Http { rpc_url }
                if rpc_url == "https://rpc.example.test/private"
        ));

        let testnet_runner =
            testnet_from_stable_callsite().rpc_url("https://testnet-rpc.example.test");
        assert_eq!(testnet_runner.network_passphrase, TESTNET_PASSPHRASE);
    }

    #[test]
    fn invalid_rpc_override_is_typed_only_when_capture_is_required_and_redacted() {
        let secret = "credential-must-not-leak";
        let result = mainnet()
            .rpc_url(format!("not-a-url-{secret}"))
            .cache(
                std::env::temp_dir()
                    .join(format!("kanatoko-invalid-rpc-{}.json", std::process::id())),
            )
            .refresh()
            .run(|_| {});

        let Err(error) = result else {
            panic!("an invalid RPC URL must fail when capture is required");
        };
        assert!(matches!(
            error,
            AutoRunError::Capture(CaptureError::InvalidRpcUrl)
        ));
        assert!(!format!("{error:?} {error}").contains(secret));
    }

    #[test]
    fn explicit_cache_overrides_public_default_while_custom_builder_stays_disabled() {
        let explicit = PathBuf::from("custom/capture.json");
        assert_eq!(
            mainnet_from_stable_callsite().cache(&explicit).cache,
            Some(explicit)
        );

        let builder =
            CaptureBuilder::mainnet(DEFAULT_MAINNET_RPC_URL).expect("built-in URL must be valid");
        assert!(AutoRunner::with_builder(builder, MAINNET_PASSPHRASE)
            .cache
            .is_none());
    }

    fn mainnet_from_stable_callsite() -> AutoRunner {
        mainnet()
    }

    fn mainnet_from_other_callsite() -> AutoRunner {
        mainnet()
    }

    fn testnet_from_stable_callsite() -> AutoRunner {
        testnet()
    }

    fn cache_hash(path: &Path) -> &str {
        path.file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .rsplit_once('-')
            .unwrap()
            .1
    }
}
