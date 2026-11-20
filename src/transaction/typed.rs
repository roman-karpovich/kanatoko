//! A small typed layer over transaction-faithful execution.
//!
//! Only XDR values cross into a [`StrictFork`], which replaces its
//! environment on every commit. Arguments and keys are [`IntoScVal`] values;
//! SDK values, such as contract types, are converted to and from XDR in
//! fresh environments of their own ([`to_sc_val`], [`from_sc_val`],
//! [`from_sc_val_in`], [`sdk`]), and a value holding an object of another
//! environment is refused. Results, contract data, and event fields are
//! decoded the same way, so no SDK object of a fork's environment escapes.

use std::{
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
};

use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext, AccountId, AlphaNum12, AlphaNum4,
    AssetCode12, AssetCode4, ContractDataDurability, ContractEventBody, ContractExecutable,
    HostFunction, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyAccount,
    LedgerKeyContractData, LedgerKeyTrustLine, PublicKey, ScAddress, ScContractInstance, ScError,
    ScMap, ScMapEntry, ScSymbol, ScVal, ScVec, SorobanCredentials, TrustLineAsset, TrustLineEntry,
    TrustLineEntryExt, Uint256,
};
use soroban_sdk::{testutils::EnvTestConfig, Env, IntoVal, TryFromVal, Val};
use thiserror::Error;

use super::{
    AuthorizationFormat, Ed25519Signer, FailedApply, PreparedTransaction, TransactionCall,
    TransactionError, TransactionReceipt,
};
use crate::{
    strict::contract_instance_key, AuthMode, AuthorizationTree, DetachedEvent, ExecutionMode,
    InvocationFailure, InvokeErrorKind, InvokeOutcome, InvokeRequest, StateChange, StrictFork,
    StrictForkError,
};

/// A value that does not convert between the SDK and XDR.
#[derive(Clone, Copy, Debug, Default, Eq, Error, PartialEq)]
#[error("the value does not convert between the SDK and XDR")]
pub struct ConversionError;

/// A fresh SDK `Env` without ledger state, used for one conversion. Every
/// conversion starts from its own environment with an unlimited budget, so
/// nothing accumulates over a thread's life and no budget runs out.
fn fresh_env() -> Env {
    let env = Env::new_with_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    let _ = env.host().budget_cloned().reset_unlimited();
    env
}

/// The handle the next object of `env` gets.
fn next_handle(env: &Env) -> u32 {
    soroban_sdk::Bytes::new(env).to_object().get_handle()
}

/// Builds and converts a value in `env`, catching a panic of `build`.
fn convert_in<T, F>(env: &Env, build: &F) -> Result<ScVal, ConversionError>
where
    T: IntoVal<Env, Val>,
    F: Fn(&Env) -> Result<T, ConversionError>,
{
    catch_unwind(AssertUnwindSafe(|| {
        let value = build(env)?;
        ScVal::try_from_val(env, &value.into_val(env)).map_err(|_| ConversionError)
    }))
    .map_err(|_| ConversionError)?
}

/// The checked conversion behind [`to_sc_val`] and [`sdk`].
///
/// An SDK object value carries only a handle into the `Env` that made it,
/// and the SDK converts an object of another `Env` by its handle alone, so
/// such a value would silently stand for whatever object has that handle
/// here. `build` therefore runs twice: once in a fresh environment, and once
/// in a fresh environment whose first handles, up to the first run's last,
/// belong to filler objects. Objects `build` makes itself get different
/// handles in the two runs and convert alike; a handle from elsewhere names
/// a different object, or none, in at least one run, so the results differ
/// or a run fails, and the value is refused.
fn checked_to_sc_val<T, F>(build: F) -> Result<ScVal, ConversionError>
where
    T: IntoVal<Env, Val>,
    F: Fn(&Env) -> Result<T, ConversionError>,
{
    let first = fresh_env();
    let value = convert_in(&first, &build)?;
    let watermark = next_handle(&first);
    let second = fresh_env();
    let mut filler = 0_u32;
    while next_handle(&second) < watermark {
        let mut bytes = *b"kanatoko conversion filler \0\0\0\0";
        bytes[27..].copy_from_slice(&filler.to_be_bytes());
        let _ = soroban_sdk::Bytes::from_array(&second, &bytes);
        filler += 1;
    }
    if convert_in(&second, &build)? == value {
        Ok(value)
    } else {
        Err(ConversionError)
    }
}

/// Converts an SDK value, such as a `#[contracttype]` struct, enum, vector,
/// or map, to XDR. `build` makes the value in the `Env` it is given; it runs
/// more than once and must make the same value each time.
///
/// Every SDK object the value holds must come from that `Env`: an
/// `Address`, `Vec`, `Map`, `Bytes`, or `String` built in any other `Env`,
/// including one captured from outside `build` or decoded by
/// [`from_sc_val`], is refused rather than converted. Decode inside `build`
/// with [`from_sc_val_in`] to change a stored value.
///
/// # Errors
///
/// Returns [`ConversionError`] when `build` panics, the value does not
/// convert, or it holds an object of another environment.
pub fn to_sc_val<T, F>(build: F) -> Result<ScVal, ConversionError>
where
    T: IntoVal<Env, Val>,
    F: Fn(&Env) -> T,
{
    checked_to_sc_val(|env| Ok(build(env)))
}

/// Converts XDR to an SDK value, such as a contract type of a
/// `contractimport!` client, `Map<Symbol, Val>`, or a tuple, in a fresh
/// `Env` that the value keeps alive. Pass a decoded value back to a fork as
/// XDR, or decode it again inside [`to_sc_val`] or [`sdk`] with
/// [`from_sc_val_in`].
///
/// # Errors
///
/// Returns [`ConversionError`] when the value has another shape.
pub fn from_sc_val<T: TryFromVal<Env, Val>>(value: &ScVal) -> Result<T, ConversionError> {
    from_sc_val_in(&fresh_env(), value)
}

/// Converts XDR to an SDK value in `env`, for example the `Env` given to a
/// [`to_sc_val`] or [`sdk`] closure.
///
/// # Errors
///
/// Returns [`ConversionError`] when the value has another shape.
pub fn from_sc_val_in<T: TryFromVal<Env, Val>>(
    env: &Env,
    value: &ScVal,
) -> Result<T, ConversionError> {
    catch_unwind(AssertUnwindSafe(|| {
        let value = Val::try_from_val(env, value).map_err(|_| ConversionError)?;
        T::try_from_val(env, &value).map_err(|_| ConversionError)
    }))
    .map_err(|_| ConversionError)?
}

/// An SDK value used as an [`IntoScVal`] argument, key, or value; see
/// [`sdk`].
#[derive(Clone)]
pub struct Sdk<F>(F);

impl<F> fmt::Debug for Sdk<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Sdk(..)")
    }
}

/// An SDK value built by `build` in the `Env` it is given, as [`to_sc_val`]
/// converts it, for example
/// `(sdk(|env| Position { owner: Address::from_str(env, OWNER), amount: 5 }), &owner)`
/// as call arguments.
#[must_use]
pub const fn sdk<T, F>(build: F) -> Sdk<F>
where
    T: IntoVal<Env, Val>,
    F: Fn(&Env) -> T,
{
    Sdk(build)
}

impl<T, F> IntoScVal for Sdk<F>
where
    T: IntoVal<Env, Val>,
    F: Fn(&Env) -> T,
{
    /// # Panics
    ///
    /// Panics if [`to_sc_val`] refuses the value, for example because it
    /// holds an object of another `Env`.
    fn into_sc_val(self) -> ScVal {
        to_sc_val(self.0).expect("an SDK value converts from the Env given to its closure")
    }
}

/// Ledgers a signature from [`StrictFork::sign`] stays valid for.
pub(crate) const SIGNATURE_LEDGERS: u32 = 100;
/// Issuer account flags that Stellar Asset Contracts read.
const AUTH_REQUIRED_FLAG: u32 = 1;
const AUTH_CLAWBACK_ENABLED_FLAG: u32 = 8;
/// Trustline flags.
const AUTHORIZED_FLAG: u32 = 1;
const TRUSTLINE_CLAWBACK_ENABLED_FLAG: u32 = 4;
/// Core's limit of account sub-entries.
const MAX_ACCOUNT_SUBENTRIES: u32 = 1000;
const NONCE_DOMAIN: &[u8] = b"KANATOKO\0AUTHORIZATION-NONCE\0V1\0";

/// A value that becomes one XDR [`ScVal`] without an `Env`.
///
/// Implemented for integers, `bool`, `()`, [`ScVal`], [`ScAddress`],
/// [`ScSymbol`], and [`Ed25519Signer`] (its account address). Use
/// [`symbol`] for a symbol and [`vec_val`] for a vector, such as a contract
/// type's enum key.
pub trait IntoScVal {
    fn into_sc_val(self) -> ScVal;
}

macro_rules! into_sc_val_via_from {
    ($($type:ty),*) => {
        $(impl IntoScVal for $type {
            fn into_sc_val(self) -> ScVal {
                ScVal::from(self)
            }
        })*
    };
}

into_sc_val_via_from!(i32, u32, i64, u64, i128, u128, bool, (), ScSymbol);

impl IntoScVal for ScVal {
    fn into_sc_val(self) -> ScVal {
        self
    }
}

impl IntoScVal for &ScVal {
    fn into_sc_val(self) -> ScVal {
        self.clone()
    }
}

impl IntoScVal for ScAddress {
    fn into_sc_val(self) -> ScVal {
        ScVal::Address(self)
    }
}

impl IntoScVal for &ScAddress {
    fn into_sc_val(self) -> ScVal {
        ScVal::Address(self.clone())
    }
}

impl IntoScVal for &Ed25519Signer {
    fn into_sc_val(self) -> ScVal {
        ScVal::Address(self.address())
    }
}

/// Contract call arguments without an `Env`: a tuple of up to ten
/// [`IntoScVal`] values, or a `Vec<ScVal>`.
pub trait IntoArgs {
    fn into_args(self) -> Vec<ScVal>;
}

impl IntoArgs for Vec<ScVal> {
    fn into_args(self) -> Vec<ScVal> {
        self
    }
}

impl IntoArgs for () {
    fn into_args(self) -> Vec<ScVal> {
        Vec::new()
    }
}

macro_rules! into_args_for_tuple {
    ($($name:ident),+) => {
        impl<$($name: IntoScVal),+> IntoArgs for ($($name,)+) {
            #[allow(non_snake_case)]
            fn into_args(self) -> Vec<ScVal> {
                let ($($name,)+) = self;
                vec![$($name.into_sc_val()),+]
            }
        }
    };
}

into_args_for_tuple!(A);
into_args_for_tuple!(A, B);
into_args_for_tuple!(A, B, C);
into_args_for_tuple!(A, B, C, D);
into_args_for_tuple!(A, B, C, D, E);
into_args_for_tuple!(A, B, C, D, E, F);
into_args_for_tuple!(A, B, C, D, E, F, G);
into_args_for_tuple!(A, B, C, D, E, F, G, H);
into_args_for_tuple!(A, B, C, D, E, F, G, H, I);
into_args_for_tuple!(A, B, C, D, E, F, G, H, I, J);

/// A Soroban symbol value.
///
/// # Panics
///
/// Panics if `name` is not a valid symbol of at most 32 characters.
#[must_use]
pub fn symbol(name: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(
        name.try_into()
            .expect("a symbol has at most 32 valid characters"),
    ))
}

/// A Soroban vector value, such as the key `vec_val((symbol("Balance"),
/// holder))` of a contract type's enum variant.
///
/// # Panics
///
/// Panics if the vector exceeds the XDR bound.
#[must_use]
pub fn vec_val(items: impl IntoArgs) -> ScVal {
    ScVal::Vec(Some(ScVec(
        items
            .into_args()
            .try_into()
            .expect("a Soroban vector fits its XDR bound"),
    )))
}

/// The upload and creation of one contract by [`StrictFork::deploy`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Deployment {
    /// The created contract.
    pub address: ScAddress,
    pub upload: TransactionReceipt,
    pub create: TransactionReceipt,
}

impl TransactionCall {
    /// `HostFunction::InvokeContract` of `function` on `contract` with
    /// `args`, for example `(&signer, 100_i128)`.
    ///
    /// # Errors
    ///
    /// Returns `TransactionError::Strict(StrictForkError::InvalidInvocationXdr)`
    /// when `function` is not a valid symbol.
    pub fn invoke_fn(
        source: &Ed25519Signer,
        contract: &ScAddress,
        function: &str,
        args: impl IntoArgs,
    ) -> Result<Self, TransactionError> {
        Self::invoke(source, &request(contract, function, args)?)
    }
}

fn request(
    contract: &ScAddress,
    function: &str,
    args: impl IntoArgs,
) -> Result<InvokeRequest, TransactionError> {
    Ok(InvokeRequest {
        contract: contract.clone(),
        function: ScSymbol(
            function
                .try_into()
                .map_err(|_| StrictForkError::InvalidInvocationXdr)?,
        ),
        args: args.into_args(),
    })
}

fn invocation_failure(error: Option<ScError>, kind: InvokeErrorKind) -> TransactionError {
    TransactionError::Invocation(InvocationFailure { error, kind })
}

fn conversion_failure() -> TransactionError {
    invocation_failure(None, InvokeErrorKind::ResultConversion)
}

fn sac_key(name: &str, holder: &ScAddress) -> ScVal {
    vec_val((symbol(name), holder))
}

/// The classic asset of a Stellar Asset Contract instance.
enum SacAsset {
    Native,
    Credit(TrustLineAsset, AccountId),
}

fn sac_asset(instance: &ScContractInstance) -> Option<SacAsset> {
    if instance.executable != ContractExecutable::StellarAsset {
        return None;
    }
    let storage = instance.storage.as_ref()?;
    let info = storage
        .iter()
        .find(|entry| entry.key == vec_val((symbol("AssetInfo"),)))?;
    let ScVal::Vec(Some(parts)) = &info.val else {
        return None;
    };
    let name = parts.first()?;
    if name == &symbol("Native") {
        return Some(SacAsset::Native);
    }
    let ScVal::Map(Some(fields)) = parts.get(1)? else {
        return None;
    };
    let field = |key: &str| {
        fields
            .iter()
            .find(|entry| entry.key == symbol(key))
            .map(|entry| &entry.val)
    };
    let ScVal::String(code) = field("asset_code")? else {
        return None;
    };
    let ScVal::Bytes(issuer) = field("issuer")? else {
        return None;
    };
    let issuer = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        issuer.as_slice().try_into().ok()?,
    )));
    let code = code.as_slice();
    let asset = if name == &symbol("AlphaNum4") {
        let mut padded = [0; 4];
        padded.get_mut(..code.len())?.copy_from_slice(code);
        TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(padded),
            issuer: issuer.clone(),
        })
    } else if name == &symbol("AlphaNum12") {
        let mut padded = [0; 12];
        padded.get_mut(..code.len())?.copy_from_slice(code);
        TrustLineAsset::CreditAlphanum12(AlphaNum12 {
            asset_code: AssetCode12(padded),
            issuer: issuer.clone(),
        })
    } else {
        return None;
    };
    Some(SacAsset::Credit(asset, issuer))
}

impl StrictFork {
    /// Calls a read-only `function` on `contract` against the last closed
    /// ledger and converts its result with [`from_sc_val`]. Nothing is
    /// committed or recorded, and no authorization is given.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::Invocation`] when the call fails or its
    /// result does not convert to `R`, and the errors of [`Self::invoke`].
    pub fn view<R>(
        &mut self,
        contract: &ScAddress,
        function: &str,
        args: impl IntoArgs,
    ) -> Result<R, TransactionError>
    where
        R: TryFromVal<Env, Val>,
    {
        let receipt = self.invoke_from(
            None,
            request(contract, function, args)?,
            ExecutionMode::Preview,
            AuthMode::Enforce(vec![]),
            false,
        )?;
        match receipt.outcome {
            InvokeOutcome::Success(value) => self.decode(&value),
            InvokeOutcome::Failure { error, kind } => Err(invocation_failure(error, kind)),
        }
    }

    #[allow(clippy::unused_self)]
    fn decode<R: TryFromVal<Env, Val>>(&self, value: &ScVal) -> Result<R, TransactionError> {
        from_sc_val(value).map_err(|_| conversion_failure())
    }

    /// The token balance of `holder`, from the token contract's `balance`.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::view`].
    pub fn token_balance(
        &mut self,
        token: &ScAddress,
        holder: &ScAddress,
    ) -> Result<i128, TransactionError> {
        self.view(token, "balance", (holder,))
    }

    /// The native balance of `account` in stroops; `None` if it does not
    /// exist.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::UnknownLedgerKeys`] for an uncaptured
    /// account.
    pub fn account_balance(&self, account: &AccountId) -> Result<Option<i64>, StrictForkError> {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account.clone(),
        });
        Ok(self
            .ledger_entry(&key)?
            .and_then(|value| match value.entry.data {
                LedgerEntryData::Account(account) => Some(account.balance),
                _ => None,
            }))
    }

    /// The instance of `contract`: its executable and instance storage.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::UnknownLedgerKeys`] for an uncaptured
    /// instance.
    pub fn contract_instance(
        &self,
        contract: &ScAddress,
    ) -> Result<Option<ScContractInstance>, StrictForkError> {
        let Some(value) = self.ledger_entry(&contract_instance_key(contract.clone()))? else {
            return Ok(None);
        };
        match value.entry.data {
            LedgerEntryData::ContractData(data) => match data.val {
                ScVal::ContractInstance(instance) => Ok(Some(instance)),
                _ => Err(StrictForkError::HostInspection),
            },
            _ => Err(StrictForkError::HostInspection),
        }
    }

    /// One contract data value, converted to `V`; `None` if absent.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::Invocation`] when the value does not
    /// convert, and [`StrictForkError::UnknownLedgerKeys`] for an uncaptured
    /// key.
    pub fn contract_data<V>(
        &self,
        contract: &ScAddress,
        key: impl IntoScVal,
        durability: ContractDataDurability,
    ) -> Result<Option<V>, TransactionError>
    where
        V: TryFromVal<Env, Val>,
    {
        self.stored_contract_data(contract, key.into_sc_val(), durability)?
            .map(|value| self.decode(&value))
            .transpose()
    }

    fn stored_contract_data(
        &self,
        contract: &ScAddress,
        key: ScVal,
        durability: ContractDataDurability,
    ) -> Result<Option<ScVal>, TransactionError> {
        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract.clone(),
            key,
            durability,
        });
        let Some(value) = self.ledger_entry(&key)? else {
            return Ok(None);
        };
        let LedgerEntryData::ContractData(data) = value.entry.data else {
            return Err(StrictForkError::HostInspection.into());
        };
        Ok(Some(data.val))
    }

    /// Explicitly rewrites one existing contract data value with `edit`, as
    /// [`Self::set_contract_data`] does. `V` may be a contract type or, for
    /// a foreign structure, `Map<Symbol, Val>`.
    ///
    /// `edit` receives the `Env` the value was decoded in; build any new SDK
    /// object it stores there, for example
    /// `|env, position| position.owner = Address::from_str(env, OWNER)`. It
    /// runs more than once, as [`to_sc_val`] describes, and an object of
    /// another `Env` is refused.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::MissingContractData`] for an absent entry,
    /// [`TransactionError::Invocation`] when the value does not convert or
    /// `edit` stores an object of another `Env`, and
    /// [`StrictForkError::UnknownLedgerKeys`] for an uncaptured key.
    pub fn update_contract_data<V>(
        &mut self,
        contract: &ScAddress,
        key: impl IntoScVal,
        durability: ContractDataDurability,
        edit: impl Fn(&Env, &mut V),
    ) -> Result<StateChange, TransactionError>
    where
        V: TryFromVal<Env, Val> + IntoVal<Env, Val>,
    {
        let key = key.into_sc_val();
        let stored = self
            .stored_contract_data(contract, key.clone(), durability)?
            .ok_or(TransactionError::MissingContractData)?;
        let encoded = checked_to_sc_val(|env| {
            let mut value: V = from_sc_val_in(env, &stored)?;
            edit(env, &mut value);
            Ok(value)
        })
        .map_err(|_| conversion_failure())?;
        Ok(self.set_contract_data(contract, key, durability, Some(encoded))?)
    }

    /// Explicitly sets the balance of `holder` in the Stellar Asset Contract
    /// `token`, as a local override for test setup, not a payment or mint.
    ///
    /// The result is a state the network can reach, with the flags the
    /// contract and Stellar Core would give it:
    ///
    /// - a contract holder's `Balance` entry keeps its flags; a new one is
    ///   authorized and has clawback when the issuer enables clawback;
    /// - an account's native balance must stay within its minimum balance
    ///   plus selling liabilities and the maximum minus buying liabilities;
    /// - an account's classic-asset balance is its trustline balance, within
    ///   the trustline's selling and buying liabilities and limit. A missing
    ///   trustline is created as the contract creates one: maximum limit,
    ///   authorized unless the issuer requires authorization, clawback when
    ///   the issuer enables it, one more account sub-entry, and only if the
    ///   account can reserve it.
    ///
    /// A balance that is not authorized cannot change.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::UnsupportedTokenBalance`] with the reason
    /// for a state the network cannot reach or a token that is not a Stellar
    /// Asset Contract, and [`StrictForkError::UnknownLedgerKeys`] for an
    /// uncaptured key, such as the issuer account.
    #[allow(clippy::too_many_lines)]
    pub fn set_token_balance(
        &mut self,
        token: &ScAddress,
        holder: &ScAddress,
        amount: i128,
    ) -> Result<StateChange, TransactionError> {
        let instance = self
            .contract_instance(token)?
            .ok_or_else(|| unsupported("the token contract does not exist"))?;
        let asset = sac_asset(&instance)
            .ok_or_else(|| unsupported("the token is not a Stellar Asset Contract"))?;
        let issuer_flags = match &asset {
            SacAsset::Native => 0,
            SacAsset::Credit(_, issuer) => {
                self.account(issuer)?
                    .ok_or_else(|| unsupported("the issuer account does not exist"))?
                    .flags
            }
        };
        let account_id = match holder {
            ScAddress::Account(account_id) => account_id.clone(),
            ScAddress::Contract(_) => {
                return self.set_contract_balance(token, holder, amount, issuer_flags)
            }
            _ => {
                return Err(unsupported(
                    "the holder is neither an account nor a contract",
                ))
            }
        };
        let amount = i64::try_from(amount)
            .ok()
            .filter(|amount| *amount >= 0)
            .ok_or_else(|| unsupported("an account balance is a non-negative i64"))?;
        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        let mut account = self
            .account(&account_id)?
            .ok_or_else(|| unsupported("the holder account does not exist"))?;
        let base_reserve = i64::from(self.ledger_info().base_reserve);
        let (asset, issuer) = match asset {
            SacAsset::Native => {
                let (minimum, maximum) = account_balance_bounds(&account, base_reserve);
                if amount < minimum || amount > maximum {
                    return Err(unsupported(
                        "the native balance leaves the account's reserve and liabilities range",
                    ));
                }
                account.balance = amount;
                return Ok(self.override_entry(
                    account_key,
                    Some((account_entry(account, self.ledger_sequence()), None)),
                )?);
            }
            SacAsset::Credit(asset, issuer) => (asset, issuer),
        };
        if issuer == account_id {
            return Err(unsupported("the issuer has no trustline to its own asset"));
        }
        let trustline_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: account_id.clone(),
            asset: asset.clone(),
        });
        let existing = self.ledger_entry(&trustline_key)?;
        let mut line = if let Some(existing) = existing {
            match existing.entry.data {
                LedgerEntryData::Trustline(line) => line,
                _ => return Err(StrictForkError::HostInspection.into()),
            }
        } else {
            if account.num_sub_entries >= MAX_ACCOUNT_SUBENTRIES {
                return Err(unsupported("the account has the maximum of sub-entries"));
            }
            let (minimum, _) = account_balance_bounds(&account, base_reserve);
            if account.balance < minimum.saturating_add(base_reserve) {
                return Err(unsupported("the account cannot reserve a new trustline"));
            }
            let mut flags = 0;
            if issuer_flags & AUTH_REQUIRED_FLAG == 0 {
                flags |= AUTHORIZED_FLAG;
            }
            if issuer_flags & AUTH_CLAWBACK_ENABLED_FLAG != 0 {
                flags |= TRUSTLINE_CLAWBACK_ENABLED_FLAG;
            }
            account.num_sub_entries += 1;
            let sequence = self.ledger_sequence();
            self.override_entry(account_key, Some((account_entry(account, sequence), None)))?;
            TrustLineEntry {
                account_id,
                asset,
                balance: 0,
                limit: i64::MAX,
                flags,
                ext: TrustLineEntryExt::V0,
            }
        };
        // Only a fully authorized trustline receives or sends; one authorized
        // to maintain liabilities keeps its balance.
        if line.flags & AUTHORIZED_FLAG == 0 && amount != line.balance {
            return Err(unsupported("the trustline is not authorized"));
        }
        let (minimum, maximum) = trustline_balance_bounds(&line)
            .ok_or_else(|| unsupported("the trustline limit is below its buying liabilities"))?;
        if amount < minimum || amount > maximum {
            return Err(unsupported(
                "the balance leaves the trustline's liabilities and limit range",
            ));
        }
        line.balance = amount;
        let entry = LedgerEntry {
            last_modified_ledger_seq: self.ledger_sequence(),
            data: LedgerEntryData::Trustline(line),
            ext: LedgerEntryExt::V0,
        };
        Ok(self.override_entry(trustline_key, Some((entry, None)))?)
    }

    fn account(&self, account_id: &AccountId) -> Result<Option<AccountEntry>, StrictForkError> {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        Ok(self
            .ledger_entry(&key)?
            .and_then(|value| match value.entry.data {
                LedgerEntryData::Account(account) => Some(account),
                _ => None,
            }))
    }

    fn set_contract_balance(
        &mut self,
        token: &ScAddress,
        holder: &ScAddress,
        amount: i128,
        issuer_flags: u32,
    ) -> Result<StateChange, TransactionError> {
        if amount < 0 {
            return Err(unsupported("a balance is not negative"));
        }
        let key = sac_key("Balance", holder);
        let durability = ContractDataDurability::Persistent;
        let existing: Option<ScVal> = self
            .ledger_entry(&LedgerKey::ContractData(LedgerKeyContractData {
                contract: token.clone(),
                key: key.clone(),
                durability,
            }))?
            .and_then(|value| match value.entry.data {
                LedgerEntryData::ContractData(data) => Some(data.val),
                _ => None,
            });
        let field = |name: &str| match &existing {
            Some(ScVal::Map(Some(map))) => map
                .iter()
                .find(|entry| entry.key == symbol(name))
                .map(|entry| entry.val.clone()),
            _ => None,
        };
        let (authorized, clawback) = if existing.is_some() {
            let current = field("amount");
            if field("authorized") == Some(ScVal::Bool(false))
                && current != Some(ScVal::from(amount))
            {
                return Err(unsupported("the contract balance is not authorized"));
            }
            (
                field("authorized").unwrap_or(ScVal::Bool(true)),
                field("clawback").unwrap_or(ScVal::Bool(false)),
            )
        } else {
            // A new balance comes from a receipt, which needs authorization:
            // the contract then writes it authorized.
            if issuer_flags & AUTH_REQUIRED_FLAG != 0 && amount != 0 {
                return Err(unsupported(
                    "the issuer requires authorization for a new balance",
                ));
            }
            (
                ScVal::Bool(true),
                ScVal::Bool(issuer_flags & AUTH_CLAWBACK_ENABLED_FLAG != 0),
            )
        };
        let value = ScVal::Map(Some(ScMap(
            vec![
                ScMapEntry {
                    key: symbol("amount"),
                    val: ScVal::from(amount),
                },
                ScMapEntry {
                    key: symbol("authorized"),
                    val: authorized,
                },
                ScMapEntry {
                    key: symbol("clawback"),
                    val: clawback,
                },
            ]
            .try_into()
            .map_err(|_| StrictForkError::Xdr)?,
        )));
        Ok(self.set_contract_data(token, key, durability, Some(value))?)
    }

    /// [`Self::sign_transaction`] with derived nonces and signatures valid
    /// for 100 ledgers.
    ///
    /// The nonce of each signed tree is SHA-256 of the network ID, the
    /// signing address, and a counter of this fork, truncated to a
    /// non-negative `i64`: unique within the fork, unlikely to meet a live
    /// nonce of a captured account, and the same on every pass of
    /// [`crate::AutoRunner::run_strict`] and every replay of a deterministic
    /// scenario.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::sign_transaction`].
    pub fn sign(
        &self,
        call: &TransactionCall,
        signers: &[&Ed25519Signer],
    ) -> Result<PreparedTransaction, TransactionError> {
        let simulation = self.simulate_transaction(call)?;
        if !simulation.receipt.outcome.is_success() {
            return Err(TransactionError::SimulationFailed(Box::new(
                simulation.receipt,
            )));
        }
        let expiration = self.ledger_sequence().saturating_add(SIGNATURE_LEDGERS);
        let authorization = self.authorize_with(
            AuthorizationFormat::Address,
            &simulation,
            signers,
            expiration,
            |_, tree| self.derived_nonce(&tree.address),
        )?;
        self.prepare_transaction(call, authorization)
    }

    fn derived_nonce(&self, address: &ScAddress) -> Result<i64, TransactionError> {
        let counter = self.next_nonce.get();
        self.next_nonce.set(
            counter
                .checked_add(1)
                .ok_or(TransactionError::NonceOverflow)?,
        );
        let mut digest = Sha256::new();
        digest.update(NONCE_DOMAIN);
        digest.update(self.network_id());
        digest.update(
            soroban_env_host::xdr::WriteXdr::to_xdr(address, soroban_env_host::xdr::Limits::none())
                .map_err(|_| StrictForkError::Xdr)?,
        );
        digest.update(counter.to_be_bytes());
        let bytes: [u8; 8] = digest.finalize()[..8]
            .try_into()
            .map_err(|_| StrictForkError::Xdr)?;
        Ok(i64::from_be_bytes(bytes) & i64::MAX)
    }

    /// Signs and applies `call`; only a committed transaction is `Ok`.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::sign`] and [`Self::apply_transaction`].
    pub fn apply_signed(
        &mut self,
        call: &TransactionCall,
        signers: &[&Ed25519Signer],
    ) -> Result<TransactionReceipt, TransactionError> {
        let transaction = self.sign(call, signers)?;
        self.apply_transaction(&transaction)
    }

    /// [`Self::create_local_account`] for every signer.
    ///
    /// # Errors
    ///
    /// The first error of [`Self::create_local_account`].
    pub fn create_local_accounts(
        &mut self,
        signers: &[&Ed25519Signer],
        balance: i64,
    ) -> Result<(), TransactionError> {
        for signer in signers {
            self.create_local_account(signer, balance)?;
        }
        Ok(())
    }

    /// Uploads `wasm` and creates a contract from it with `CreateContractV2`
    /// at the address `deployer` and `salt` derive, running its constructor
    /// with `constructor_args`; both transactions are signed by and paid
    /// from `deployer`.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::apply_signed`].
    pub fn deploy(
        &mut self,
        deployer: &Ed25519Signer,
        wasm: &[u8],
        salt: [u8; 32],
        constructor_args: impl IntoArgs,
    ) -> Result<Deployment, TransactionError> {
        let upload = TransactionCall::upload_wasm(deployer, wasm)?;
        let upload = self.apply_signed(&upload, &[deployer])?;
        let create = TransactionCall::create_contract(
            deployer,
            deployer.address(),
            salt,
            Sha256::digest(wasm).into(),
            constructor_args.into_args(),
        )?;
        let create = self.apply_signed(&create, &[deployer])?;
        let InvokeOutcome::Success(ScVal::Address(address)) = &create.outcome else {
            return Err(StrictForkError::HostInspection.into());
        };
        Ok(Deployment {
            address: address.clone(),
            upload,
            create,
        })
    }

    /// [`Self::expect_failed_apply`] that also requires the transaction to be
    /// included and fail with exactly `expected` from the Host; the fork
    /// then keeps only the fee and sequence-number change. On any error the
    /// fork is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::UnexpectedFailure`] for another failure,
    /// and the errors of [`Self::expect_failed_apply`].
    ///
    /// # Panics
    ///
    /// Never in practice: the checkpoint belongs to this fork.
    pub fn expect_rejected_with(
        &mut self,
        transaction: &PreparedTransaction,
        expected: &ScError,
    ) -> Result<TransactionReceipt, TransactionError> {
        let checkpoint = self.checkpoint();
        let failure = self.expect_failed_apply(transaction)?;
        match failure {
            FailedApply::Rejected(receipt) if failure_error(&receipt) == Some(expected) => {
                Ok(*receipt)
            }
            other => {
                self.revert(checkpoint)
                    .expect("a fork's own checkpoint always reverts");
                Err(TransactionError::UnexpectedFailure(Box::new(other)))
            }
        }
    }

    /// Runs `body` and then reverts the fork to its state before, for
    /// example to try an alternative without affecting later steps. The
    /// revert clears the receipt history of [`Self::invoke`].
    ///
    /// # Panics
    ///
    /// Never in practice: the checkpoint belongs to this fork.
    pub fn scoped<R>(&mut self, body: impl FnOnce(&mut Self) -> R) -> R {
        let checkpoint = self.checkpoint();
        let result = body(self);
        self.revert(checkpoint)
            .expect("a fork's own checkpoint always reverts");
        result
    }
}

fn unsupported(reason: &'static str) -> TransactionError {
    TransactionError::UnsupportedTokenBalance { reason }
}

fn account_entry(account: AccountEntry, sequence: u32) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: sequence,
        data: LedgerEntryData::Account(account),
        ext: LedgerEntryExt::V0,
    }
}

/// The Host's `get_min_max_account_balance`: the minimum balance plus
/// selling liabilities, and the maximum minus buying liabilities.
fn account_balance_bounds(account: &AccountEntry, base_reserve: i64) -> (i64, i64) {
    let (entries, selling, buying) = match &account.ext {
        AccountEntryExt::V0 => (2 + i64::from(account.num_sub_entries), 0, 0),
        AccountEntryExt::V1(v1) => {
            let entries = match &v1.ext {
                AccountEntryExtensionV1Ext::V0 => 2 + i64::from(account.num_sub_entries),
                AccountEntryExtensionV1Ext::V2(v2) => {
                    2 + i64::from(account.num_sub_entries) + i64::from(v2.num_sponsoring)
                        - i64::from(v2.num_sponsored)
                }
            };
            (entries, v1.liabilities.selling, v1.liabilities.buying)
        }
    };
    (
        entries.saturating_mul(base_reserve).saturating_add(selling),
        i64::MAX.saturating_sub(buying),
    )
}

/// The Host's `get_min_max_trustline_balance`; `None` when the limit is
/// below the buying liabilities.
fn trustline_balance_bounds(line: &TrustLineEntry) -> Option<(i64, i64)> {
    match &line.ext {
        TrustLineEntryExt::V0 => Some((0, line.limit)),
        TrustLineEntryExt::V1(v1) => (line.limit >= v1.liabilities.buying)
            .then(|| (v1.liabilities.selling, line.limit - v1.liabilities.buying)),
    }
}

impl PreparedTransaction {
    /// The same transaction with the signature of authorization entry
    /// `index` replaced by one from `signer`, keeping the credentials kind,
    /// nonce, expiration, and any delegates; for example a wrong signer.
    ///
    /// # Errors
    ///
    /// Returns [`StrictForkError::Xdr`] if the payload cannot be encoded.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of range or the entry has source-account
    /// credentials.
    pub fn resigned_by(
        mut self,
        fork: &StrictFork,
        index: usize,
        signer: &Ed25519Signer,
    ) -> Result<Self, TransactionError> {
        let entry = self
            .authorization
            .get_mut(index)
            .expect("authorization entry index out of range");
        let invocation = entry.root_invocation.clone();
        // AddressV2 and delegated credentials sign the address-bound payload.
        let (format, credentials) = match &mut entry.credentials {
            SorobanCredentials::Address(credentials) => (AuthorizationFormat::Address, credentials),
            SorobanCredentials::AddressV2(credentials) => {
                (AuthorizationFormat::AddressV2, credentials)
            }
            SorobanCredentials::AddressWithDelegates(credentials) => (
                AuthorizationFormat::AddressV2,
                &mut credentials.address_credentials,
            ),
            SorobanCredentials::SourceAccount => {
                panic!("source-account credentials carry no signature")
            }
        };
        let tree = AuthorizationTree {
            address: credentials.address.clone(),
            invocation,
        };
        let replacement = fork.sign_authorization_as(
            format,
            &tree,
            signer,
            credentials.nonce,
            credentials.signature_expiration_ledger,
        )?;
        let (SorobanCredentials::Address(fresh) | SorobanCredentials::AddressV2(fresh)) =
            replacement.credentials
        else {
            unreachable!("signing produces address credentials");
        };
        credentials.signature = fresh.signature;
        Ok(self)
    }

    /// The same transaction with `function` and the original declarations,
    /// for a call whose own simulation fails but that Core would still
    /// include and fail.
    #[must_use]
    pub fn with_function(mut self, function: HostFunction) -> Self {
        self.call.function = function;
        self
    }

    /// The same contract call with `args`.
    ///
    /// # Panics
    ///
    /// Panics unless the function is `HostFunction::InvokeContract` or the
    /// arguments exceed the XDR bound.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoArgs) -> Self {
        let HostFunction::InvokeContract(invoke) = &mut self.call.function else {
            panic!("only a contract call has arguments");
        };
        invoke.args = args
            .into_args()
            .try_into()
            .expect("arguments fit their XDR bound");
        self
    }
}

impl DetachedEvent {
    /// Topic `index` converted by [`from_sc_val`].
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError`] for a missing topic or another shape.
    pub fn topic_as<T: TryFromVal<Env, Val>>(&self, index: usize) -> Result<T, ConversionError> {
        from_sc_val(self.topics().get(index).ok_or(ConversionError)?)
    }

    /// The data converted by [`from_sc_val`].
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError`] for another shape.
    pub fn data_as<T: TryFromVal<Env, Val>>(&self) -> Result<T, ConversionError> {
        from_sc_val(self.data())
    }

    /// The event's topics.
    #[must_use]
    pub fn topics(&self) -> &[ScVal] {
        let ContractEventBody::V0(body) = &self.event.body;
        body.topics.as_slice()
    }

    /// The event's data.
    #[must_use]
    pub fn data(&self) -> &ScVal {
        let ContractEventBody::V0(body) = &self.event.body;
        &body.data
    }

    /// Whether `contract` emitted the event and its first topic is the
    /// symbol `topic`.
    #[must_use]
    pub fn is_from(&self, contract: &ScAddress, topic: &str) -> bool {
        let ScAddress::Contract(id) = contract else {
            return false;
        };
        self.event.contract_id.as_ref() == Some(id) && self.topics().first() == Some(&symbol(topic))
    }
}

impl TransactionReceipt {
    /// The events `contract` emitted whose first topic is the symbol
    /// `topic`, in order.
    #[must_use]
    pub fn events_from(&self, contract: &ScAddress, topic: &str) -> Vec<&DetachedEvent> {
        self.events
            .iter()
            .filter(|event| event.is_from(contract, topic))
            .collect()
    }

    /// The one event of [`Self::events_from`].
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::EventCount`] unless exactly one event
    /// matches.
    pub fn single_event(
        &self,
        contract: &ScAddress,
        topic: &str,
    ) -> Result<&DetachedEvent, TransactionError> {
        match self.events_from(contract, topic).as_slice() {
            [event] => Ok(event),
            events => Err(TransactionError::EventCount {
                found: events.len(),
            }),
        }
    }
}

impl super::ResourceReport {
    /// Checks the measured resources against `limits`, for example the
    /// network limits or [`super::TransactionLimits::scaled`] to require
    /// headroom.
    ///
    /// # Errors
    ///
    /// Returns the first resource above its limit.
    pub fn check_within(
        &self,
        limits: &super::TransactionLimits,
    ) -> Result<(), super::ResourceExcess> {
        use super::TransactionResource as Resource;
        for (resource, limit, used) in [
            (
                Resource::Instructions,
                limits.instructions,
                self.instructions,
            ),
            (
                Resource::MemoryBytes,
                limits.memory_bytes,
                self.memory_bytes,
            ),
            (
                Resource::FootprintEntries,
                u64::from(limits.footprint_entries),
                u64::from(self.footprint_entries),
            ),
            (
                Resource::DiskReadEntries,
                u64::from(limits.disk_read_entries),
                u64::from(self.disk_read_entries),
            ),
            (
                Resource::DiskReadBytes,
                u64::from(limits.disk_read_bytes),
                u64::from(self.disk_read_bytes),
            ),
            (
                Resource::WriteEntries,
                u64::from(limits.write_entries),
                u64::from(self.write_entries),
            ),
            (
                Resource::WriteBytes,
                u64::from(limits.write_bytes),
                u64::from(self.write_bytes),
            ),
            (
                Resource::ContractEventsSizeBytes,
                u64::from(limits.contract_events_size_bytes),
                u64::from(self.contract_events_size_bytes),
            ),
            (
                Resource::TransactionSizeBytes,
                u64::from(limits.transaction_size_bytes),
                u64::from(self.transaction_size_bytes),
            ),
        ] {
            if used > limit {
                return Err(super::ResourceExcess {
                    resource,
                    limit,
                    used,
                });
            }
        }
        Ok(())
    }
}

impl super::TransactionLimits {
    /// These limits scaled to `percent`, rounded down; for example
    /// `limits.scaled(80)` to require 20% headroom.
    #[must_use]
    pub fn scaled(&self, percent: u32) -> Self {
        let scale64 = |value: u64| value.saturating_mul(u64::from(percent)) / 100;
        let scale32 = |value: u32| u32::try_from(scale64(u64::from(value))).unwrap_or(u32::MAX);
        Self {
            instructions: scale64(self.instructions),
            memory_bytes: scale64(self.memory_bytes),
            footprint_entries: scale32(self.footprint_entries),
            disk_read_entries: scale32(self.disk_read_entries),
            disk_read_bytes: scale32(self.disk_read_bytes),
            write_entries: scale32(self.write_entries),
            write_bytes: scale32(self.write_bytes),
            contract_events_size_bytes: scale32(self.contract_events_size_bytes),
            transaction_size_bytes: scale32(self.transaction_size_bytes),
            ledger_key_size_bytes: scale32(self.ledger_key_size_bytes),
            ledger_entry_size_bytes: scale32(self.ledger_entry_size_bytes),
            contract_code_size_bytes: scale32(self.contract_code_size_bytes),
        }
    }
}

impl fmt::Display for TransactionReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let resources = &self.resources;
        write!(
            formatter,
            "{:?} {:?} in ledger {}: fee {} stroops (non-refundable {}, refundable {}, rent {}), \
             {} instructions, {} B read, {} B written, {} B events, {} B envelope",
            self.disposition,
            self.result,
            self.ledger_sequence,
            self.fee_charged,
            resources.fee.non_refundable,
            resources.fee.refundable,
            resources.fee.rent,
            resources.instructions,
            resources.disk_read_bytes,
            resources.write_bytes,
            resources.contract_events_size_bytes,
            resources.transaction_size_bytes,
        )
    }
}

fn failure_error(receipt: &TransactionReceipt) -> Option<&ScError> {
    match &receipt.outcome {
        InvokeOutcome::Failure { error, .. } => error.as_ref(),
        InvokeOutcome::Success(_) => None,
    }
}

impl FailedApply {
    /// The Host error of a rejected transaction, if the Host reported one.
    #[must_use]
    pub fn host_error(&self) -> Option<&ScError> {
        match self {
            Self::Rejected(receipt) => failure_error(receipt),
            Self::Invalid(_) => None,
        }
    }
}
