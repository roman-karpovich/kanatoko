//! One-scenario automatic capture and strict replay.

use std::{
    fs,
    path::{Path, PathBuf},
};

use soroban_sdk::{Address, Env, IntoVal, Symbol, TryFromVal, Val, Vec as SorobanVec};
use thiserror::Error;

use crate::{capture::MAINNET_PASSPHRASE, CaptureBuilder, CaptureError, CapturedFixture};

const DEFAULT_MAINNET_RPC_URL: &str = "https://mainnet.sorobanrpc.com";

/// Creates an automatic mainnet runner rooted at `root_contract`.
///
/// The same scenario closure performs dependency discovery and the final
/// strict replay. No separate capture scenario is required.
///
/// # Panics
///
/// Panics only if Kanatoko's built-in mainnet RPC URL is not a valid HTTPS
/// origin, which would be an internal library invariant violation.
#[must_use]
pub fn mainnet(root_contract: impl Into<String>) -> AutoRunner {
    let root_contract = root_contract.into();
    let builder = CaptureBuilder::mainnet(DEFAULT_MAINNET_RPC_URL, root_contract.clone())
        .expect("the built-in mainnet RPC URL must have a valid HTTPS origin");
    AutoRunner::with_builder(builder, MAINNET_PASSPHRASE, root_contract)
}

/// Runs one repeatable scenario through automatic discovery and strict replay.
pub struct AutoRunner {
    builder: CaptureBuilder,
    network_passphrase: String,
    root_contract: String,
    cache: Option<PathBuf>,
    offline: bool,
    refresh: bool,
}

impl AutoRunner {
    pub(crate) fn with_builder(
        builder: CaptureBuilder,
        network_passphrase: impl Into<String>,
        root_contract: impl Into<String>,
    ) -> Self {
        Self {
            builder,
            network_passphrase: network_passphrase.into(),
            root_contract: root_contract.into(),
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
    /// Returns a capture, cache identity, offline-cache, or strict replay
    /// error. Scenario panics remain opaque at this boundary.
    pub fn run<F>(&self, scenario: F) -> Result<AutoRun, AutoRunError>
    where
        F: for<'a> Fn(&ScenarioFork<'a>),
    {
        let cache_existed = self.cache.as_deref().is_some_and(Path::exists);
        if let Some(path) = self.cache.as_deref().filter(|_| !self.refresh) {
            if path.exists() {
                let cached = CapturedFixture::from_file(path, &self.network_passphrase)?;
                self.validate_root(&cached)?;
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

        let captured = self.builder.capture(|env, root| {
            scenario(&ScenarioFork { env, root });
        })?;
        self.validate_root(&captured)?;
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

    fn validate_root(&self, fixture: &CapturedFixture) -> Result<(), AutoRunError> {
        if fixture.root_contract() == self.root_contract {
            Ok(())
        } else {
            Err(AutoRunError::CacheRootMismatch {
                expected: self.root_contract.clone(),
                found: fixture.root_contract().to_string(),
            })
        }
    }
}

fn replay<F>(fixture: &CapturedFixture, scenario: &F) -> Result<(), CaptureError>
where
    F: for<'a> Fn(&ScenarioFork<'a>),
{
    fixture.replay(|env, root| {
        scenario(&ScenarioFork { env, root });
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
    root: &'a Address,
}

impl<'a> ScenarioFork<'a> {
    /// Current pass environment for generated Soroban clients.
    #[must_use]
    pub const fn env(&self) -> &'a Env {
        self.env
    }

    /// Captured root address in the current pass environment.
    #[must_use]
    pub const fn root(&self) -> &'a Address {
        self.root
    }

    /// Parses a contract `StrKey` in the current pass environment.
    #[must_use]
    pub fn contract(&self, contract: &str) -> Address {
        Address::from_str(self.env, contract)
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
    #[error("capture cache root mismatch: expected {expected}, found {found}")]
    CacheRootMismatch { expected: String, found: String },
}
