//! A small typed layer over transaction-faithful execution.
//!
//! Arguments and keys are XDR values built without an `Env` ([`IntoScVal`],
//! [`IntoArgs`]): a [`StrictFork`] replaces its environment on every commit,
//! so SDK values bound to another environment could not cross into it.
//! Results are converted with the SDK's `TryFromVal` in the fork's current
//! environment.

use std::fmt;

use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    AccountEntry, AccountId, AlphaNum12, AlphaNum4, AssetCode12, AssetCode4,
    ContractDataDurability, ContractEventBody, ContractExecutable, HostFunction, LedgerEntry,
    LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyAccount, LedgerKeyContractData,
    LedgerKeyTrustLine, PublicKey, ScAddress, ScContractInstance, ScError, ScMap, ScMapEntry,
    ScSymbol, ScVal, ScVec, SorobanCredentials, TrustLineAsset, TrustLineEntry, TrustLineEntryExt,
    Uint256,
};
use soroban_sdk::{Env, IntoVal, TryFromVal, Val};

use super::{
    AuthorizationFormat, Ed25519Signer, FailedApply, PreparedTransaction, TransactionCall,
    TransactionError, TransactionReceipt,
};
use crate::{
    strict::contract_instance_key, AuthMode, AuthorizationTree, DetachedEvent, ExecutionMode,
    InvocationFailure, InvokeErrorKind, InvokeOutcome, InvokeRequest, StateChange, StrictFork,
    StrictForkError,
};

/// Ledgers a signature from [`StrictFork::sign`] stays valid for.
pub(crate) const SIGNATURE_LEDGERS: u32 = 100;
/// The trustline flag that authorizes holding and transacting an asset.
const AUTHORIZED_FLAG: u32 = 1;

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
    /// ledger and converts its result. Nothing is committed or recorded,
    /// and no authorization is given.
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

    fn decode<R: TryFromVal<Env, Val>>(&self, value: &ScVal) -> Result<R, TransactionError> {
        let value = Val::try_from_val(&self.env, value).map_err(|_| conversion_failure())?;
        R::try_from_val(&self.env, &value).map_err(|_| conversion_failure())
    }

    fn encode_value<V: IntoVal<Env, Val>>(&self, value: &V) -> Result<ScVal, TransactionError> {
        ScVal::try_from_val(&self.env, &value.into_val(&self.env)).map_err(|_| conversion_failure())
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
        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract.clone(),
            key: key.into_sc_val(),
            durability,
        });
        let Some(value) = self.ledger_entry(&key)? else {
            return Ok(None);
        };
        let LedgerEntryData::ContractData(data) = value.entry.data else {
            return Err(StrictForkError::HostInspection.into());
        };
        self.decode(&data.val).map(Some)
    }

    /// Explicitly rewrites one existing contract data value with `edit`, as
    /// [`Self::set_contract_data`] does. `V` may be a contract type or, for
    /// a foreign structure, `Map<Symbol, Val>`.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::Invocation`] when the value does not
    /// convert, and [`StrictForkError::UnknownLedgerKeys`] for an uncaptured
    /// or absent key.
    pub fn update_contract_data<V>(
        &mut self,
        contract: &ScAddress,
        key: impl IntoScVal,
        durability: ContractDataDurability,
        edit: impl FnOnce(&mut V),
    ) -> Result<StateChange, TransactionError>
    where
        V: TryFromVal<Env, Val> + IntoVal<Env, Val>,
    {
        let key = key.into_sc_val();
        let mut value: V = self
            .contract_data(contract, &key, durability)?
            .ok_or_else(|| invocation_failure(None, InvokeErrorKind::Abort))?;
        edit(&mut value);
        let encoded = self.encode_value(&value)?;
        Ok(self.set_contract_data(contract, key, durability, Some(encoded))?)
    }

    /// Explicitly sets the balance of `holder` in the Stellar Asset Contract
    /// `token`, as a local override for test setup, not a payment or mint.
    ///
    /// A contract holder's `Balance` entry is written, keeping its flags.
    /// An account holder's native balance is its account balance; for a
    /// classic asset, its trustline balance, creating an authorized trustline
    /// with the maximum limit when the account has none.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::UnsupportedTokenBalance`] for a token that
    /// is not a Stellar Asset Contract, a missing account, the issuer, or an
    /// amount out of range, and [`StrictForkError::UnknownLedgerKeys`] for an
    /// uncaptured key.
    pub fn set_token_balance(
        &mut self,
        token: &ScAddress,
        holder: &ScAddress,
        amount: i128,
    ) -> Result<StateChange, TransactionError> {
        let unsupported = |reason| TransactionError::UnsupportedTokenBalance { reason };
        let instance = self
            .contract_instance(token)?
            .ok_or_else(|| unsupported("the token contract does not exist"))?;
        let asset = sac_asset(&instance)
            .ok_or_else(|| unsupported("the token is not a Stellar Asset Contract"))?;
        let account_id = match holder {
            ScAddress::Account(account_id) => account_id.clone(),
            ScAddress::Contract(_) => return self.set_contract_balance(token, holder, amount),
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
        let mut account_entry = self
            .ledger_entry(&account_key)?
            .ok_or_else(|| unsupported("the holder account does not exist"))?
            .entry;
        let LedgerEntryData::Account(account) = &mut account_entry.data else {
            return Err(StrictForkError::HostInspection.into());
        };
        let (asset, issuer) = match asset {
            SacAsset::Native => {
                account.balance = amount;
                return Ok(self.override_entry(account_key, Some((account_entry, None)))?);
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
        let mut trustline = if let Some(existing) = existing {
            existing.entry
        } else {
            account_sub_entry_added(account);
            self.override_entry(account_key, Some((account_entry, None)))?;
            LedgerEntry {
                last_modified_ledger_seq: self.ledger_sequence(),
                data: LedgerEntryData::Trustline(TrustLineEntry {
                    account_id,
                    asset,
                    balance: 0,
                    limit: i64::MAX,
                    flags: AUTHORIZED_FLAG,
                    ext: TrustLineEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            }
        };
        if let LedgerEntryData::Trustline(line) = &mut trustline.data {
            if amount > line.limit {
                return Err(unsupported("the amount exceeds the trustline limit"));
            }
            line.balance = amount;
        }
        Ok(self.override_entry(trustline_key, Some((trustline, None)))?)
    }

    fn set_contract_balance(
        &mut self,
        token: &ScAddress,
        holder: &ScAddress,
        amount: i128,
    ) -> Result<StateChange, TransactionError> {
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
        let flag = |name: &str, default: bool| {
            match &existing {
                Some(ScVal::Map(Some(map))) => map
                    .iter()
                    .find(|entry| entry.key == symbol(name))
                    .map(|entry| entry.val.clone()),
                _ => None,
            }
            .unwrap_or(ScVal::Bool(default))
        };
        let value = ScVal::Map(Some(ScMap(
            vec![
                ScMapEntry {
                    key: symbol("amount"),
                    val: ScVal::from(amount),
                },
                ScMapEntry {
                    key: symbol("authorized"),
                    val: flag("authorized", true),
                },
                ScMapEntry {
                    key: symbol("clawback"),
                    val: flag("clawback", false),
                },
            ]
            .try_into()
            .map_err(|_| StrictForkError::Xdr)?,
        )));
        Ok(self.set_contract_data(token, key, durability, Some(value))?)
    }

    /// [`Self::sign_transaction`] with unique nonces from a counter of this
    /// fork and signatures valid for 100 ledgers.
    ///
    /// The counter starts at 1 on every fork, so a deterministic scenario
    /// signs the same nonces on every pass of
    /// [`crate::AutoRunner::run_strict`].
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
        let first_nonce = self.next_nonce.get();
        let trees = i64::try_from(simulation.authorization.len())
            .map_err(|_| TransactionError::NonceOverflow)?;
        self.next_nonce.set(
            first_nonce
                .checked_add(trees)
                .ok_or(TransactionError::NonceOverflow)?,
        );
        let expiration = self.ledger_sequence().saturating_add(SIGNATURE_LEDGERS);
        let authorization = self.authorize(&simulation, signers, first_nonce, expiration)?;
        self.prepare_transaction(call, authorization)
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

fn account_sub_entry_added(account: &mut AccountEntry) {
    account.num_sub_entries = account.num_sub_entries.saturating_add(1);
}

impl PreparedTransaction {
    /// The same transaction with authorization entry `index` signed again
    /// by `signer`, keeping its credentials format, nonce, and expiration;
    /// for example a wrong signer.
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
            .get(index)
            .expect("authorization entry index out of range");
        let (format, credentials) = match &entry.credentials {
            SorobanCredentials::Address(credentials) => (AuthorizationFormat::Address, credentials),
            SorobanCredentials::AddressV2(credentials) => {
                (AuthorizationFormat::AddressV2, credentials)
            }
            SorobanCredentials::AddressWithDelegates(credentials) => (
                AuthorizationFormat::AddressV2,
                &credentials.address_credentials,
            ),
            SorobanCredentials::SourceAccount => {
                panic!("source-account credentials carry no signature")
            }
        };
        let tree = AuthorizationTree {
            address: credentials.address.clone(),
            invocation: entry.root_invocation.clone(),
        };
        let entry = fork.sign_authorization_as(
            format,
            &tree,
            signer,
            credentials.nonce,
            credentials.signature_expiration_ledger,
        )?;
        self.authorization[index] = entry;
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

impl FailedApply {
    /// The Host error of a rejected transaction, if the Host reported one.
    #[must_use]
    pub fn host_error(&self) -> Option<&ScError> {
        match self {
            Self::Rejected(receipt) => match &receipt.outcome {
                InvokeOutcome::Failure { error, .. } => error.as_ref(),
                InvokeOutcome::Success(_) => None,
            },
            Self::Invalid(_) => None,
        }
    }
}
