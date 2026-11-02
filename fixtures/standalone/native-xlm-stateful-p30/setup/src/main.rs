//! Prepares a local standalone Protocol 30 network (stellar/quickstart) for the
//! Kanatoko Protocol 30 capture fixture.
//!
//! Usage: kanatoko-p30-standalone-setup <rpc-url> <friendbot-url> <stateful.wasm>
//!
//! Deterministic: the deployer key and contract salt are derived from fixed
//! labels, so on any fresh standalone network the same addresses result.

use std::{env, thread, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use stellar_xdr::{
    AccountId, Asset, ContractDataDurability, ContractExecutable, ContractId, ContractIdPreimage,
    ContractIdPreimageFromAddress, CreateContractArgsV2, DecoratedSignature, Hash, HashIdPreimage,
    HashIdPreimageContractId, HostFunction, InvokeContractArgs, InvokeHostFunctionOp, LedgerEntryData,
    LedgerKey, LedgerKeyAccount, LedgerKeyContractData, Limits, Memo, MuxedAccount, Operation,
    OperationBody, Preconditions, PublicKey, ReadXdr, ScAddress, ScSymbol, ScVal, SequenceNumber,
    Signature, SignatureHint, SorobanAuthorizationEntry, SorobanTransactionData, Transaction,
    TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

const PASSPHRASE: &str = "Standalone Network ; February 2017";
const DEPLOYER_LABEL: &[u8] = b"kanatoko protocol 30 standalone fixture deployer";
const STATEFUL_SALT_LABEL: &[u8] = b"kanatoko protocol 30 stateful fixture";

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn rpc(url: &str, method: &str, params: Value) -> Value {
    let response: Value = ureq::post(url)
        .send_json(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .unwrap_or_else(|error| panic!("{method}: {error}"))
        .into_json()
        .unwrap();
    if let Some(error) = response.get("error") {
        panic!("{method}: {error}");
    }
    response["result"].clone()
}

fn b64<T: WriteXdr>(value: &T) -> String {
    STANDARD.encode(value.to_xdr(Limits::none()).unwrap())
}

fn from_b64<T: ReadXdr>(value: &str) -> T {
    T::from_xdr(STANDARD.decode(value).unwrap(), Limits::none()).unwrap()
}

struct Network {
    rpc_url: String,
    network_id: [u8; 32],
    key: SigningKey,
}

impl Network {
    fn account_id(&self) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            self.key.verifying_key().to_bytes(),
        )))
    }

    fn entry(&self, key: &LedgerKey) -> Option<LedgerEntryData> {
        let result = rpc(&self.rpc_url, "getLedgerEntries", json!({"keys": [b64(key)]}));
        let entries = result["entries"].as_array()?;
        let entry = entries.first()?;
        Some(from_b64(entry["xdr"].as_str().unwrap()))
    }

    fn sequence(&self) -> i64 {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: self.account_id(),
        });
        match self.entry(&key) {
            Some(LedgerEntryData::Account(account)) => account.seq_num.0,
            other => panic!("deployer account missing: {other:?}"),
        }
    }

    fn invoke(&self, host_function: HostFunction) -> Value {
        let mut tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(self.key.verifying_key().to_bytes())),
            fee: 100,
            seq_num: SequenceNumber(self.sequence() + 1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: VecM::try_from(vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    host_function,
                    auth: VecM::default(),
                }),
            }])
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let unsigned = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: tx.clone(),
            signatures: VecM::default(),
        });
        let simulation = rpc(
            &self.rpc_url,
            "simulateTransaction",
            json!({"transaction": b64(&unsigned)}),
        );
        if let Some(error) = simulation.get("error") {
            panic!("simulation failed: {error}");
        }
        let data: SorobanTransactionData =
            from_b64(simulation["transactionData"].as_str().unwrap());
        let min_fee: u32 = simulation["minResourceFee"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let auth: Vec<SorobanAuthorizationEntry> = simulation["results"][0]["auth"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| from_b64(entry.as_str().unwrap()))
                    .collect()
            })
            .unwrap_or_default();
        let mut operations = tx.operations.to_vec();
        let OperationBody::InvokeHostFunction(op) = &mut operations[0].body else {
            unreachable!()
        };
        op.auth = VecM::try_from(auth).unwrap();
        tx.operations = VecM::try_from(operations).unwrap();
        tx.fee = 100 + min_fee * 2;
        tx.ext = TransactionExt::V1(data);

        let hash = tx.hash(self.network_id).unwrap();
        let signature = self.key.sign(&hash);
        let public = self.key.verifying_key().to_bytes();
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::try_from(vec![DecoratedSignature {
                hint: SignatureHint(public[28..].try_into().unwrap()),
                signature: Signature(signature.to_bytes().to_vec().try_into().unwrap()),
            }])
            .unwrap(),
        });
        let sent = rpc(
            &self.rpc_url,
            "sendTransaction",
            json!({"transaction": b64(&envelope)}),
        );
        assert_eq!(sent["status"], "PENDING", "send failed: {sent}");
        let hash_hex = sent["hash"].as_str().unwrap().to_string();
        for _ in 0..60 {
            thread::sleep(Duration::from_secs(1));
            let status = rpc(&self.rpc_url, "getTransaction", json!({"hash": hash_hex}));
            match status["status"].as_str() {
                Some("SUCCESS") => {
                    eprintln!("tx {hash_hex} SUCCESS at ledger {}", status["ledger"]);
                    return status;
                }
                Some("NOT_FOUND") => {}
                _ => panic!("tx {hash_hex} failed: {status}"),
            }
        }
        panic!("tx {hash_hex} not confirmed");
    }

    fn contract_id(&self, preimage: ContractIdPreimage) -> ContractId {
        let preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
            network_id: Hash(self.network_id),
            contract_id_preimage: preimage,
        });
        ContractId(Hash(sha256(&preimage.to_xdr(Limits::none()).unwrap())))
    }

    fn instance_exists(&self, contract: &ContractId) -> bool {
        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(contract.clone()),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
        });
        self.entry(&key).is_some()
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let [_, rpc_url, friendbot_url, wasm_path] = args.as_slice() else {
        panic!("usage: <rpc-url> <friendbot-url> <stateful.wasm>");
    };
    let network = Network {
        rpc_url: rpc_url.clone(),
        network_id: sha256(PASSPHRASE.as_bytes()),
        key: SigningKey::from_bytes(&sha256(DEPLOYER_LABEL)),
    };
    let passphrase = rpc(rpc_url, "getNetwork", json!({}));
    assert_eq!(passphrase["passphrase"], PASSPHRASE);
    assert_eq!(passphrase["protocolVersion"], 30);

    let deployer = network.account_id();
    let account_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: deployer.clone(),
    });
    if network.entry(&account_key).is_none() {
        ureq::get(friendbot_url)
            .query("addr", &deployer.to_string())
            .call()
            .unwrap();
        eprintln!("funded {deployer}");
    }

    let native = network.contract_id(ContractIdPreimage::Asset(Asset::Native));
    if !network.instance_exists(&native) {
        network.invoke(HostFunction::CreateContract(stellar_xdr::CreateContractArgs {
            contract_id_preimage: ContractIdPreimage::Asset(Asset::Native),
            executable: ContractExecutable::StellarAsset,
        }));
    }

    let wasm = std::fs::read(wasm_path).unwrap();
    let wasm_hash = Hash(sha256(&wasm));
    network.invoke(HostFunction::UploadContractWasm(wasm.try_into().unwrap()));

    let stateful_preimage = ContractIdPreimage::Address(ContractIdPreimageFromAddress {
        address: ScAddress::Account(deployer.clone()),
        salt: Uint256(sha256(STATEFUL_SALT_LABEL)),
    });
    let stateful = network.contract_id(stateful_preimage.clone());
    if !network.instance_exists(&stateful) {
        network.invoke(HostFunction::CreateContractV2(CreateContractArgsV2 {
            contract_id_preimage: stateful_preimage,
            executable: ContractExecutable::Wasm(wasm_hash),
            constructor_args: VecM::try_from(vec![ScVal::I64(41)]).unwrap(),
        }));
        network.invoke(HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: ScAddress::Contract(stateful.clone()),
            function_name: ScSymbol("increment".try_into().unwrap()),
            args: VecM::try_from(vec![ScVal::I64(1)]).unwrap(),
        }));
    }

    let latest = rpc(rpc_url, "getLatestLedger", json!({}));
    println!("deployer={deployer}");
    println!("native_sac={}", ScAddress::Contract(native));
    println!("stateful={}", ScAddress::Contract(stateful));
    println!(
        "latest_ledger={} protocol={}",
        latest["sequence"], latest["protocolVersion"]
    );
}
