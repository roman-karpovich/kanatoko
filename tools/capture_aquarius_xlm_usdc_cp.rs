//! Capture the narrow Aquarius XLM/USDC constant-product scenario fixture.
//!
//! This is deliberately scenario-specific tooling, not a general RPC-backed
//! fork. Normal tests never enable the `capture` feature or contact a network.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use kanatoko::{canonical_ledger_digest, SUPPORTED_PROTOCOL_VERSION};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    ConfigSettingEntry, ConfigSettingId, ContractDataDurability, ContractExecutable, Hash,
    LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerHeader, LedgerKey, LedgerKeyAccount,
    LedgerKeyConfigSetting, LedgerKeyContractCode, LedgerKeyContractData, Limits, ReadXdr,
    ScAddress, ScVal, WriteXdr,
};
use soroban_ledger_snapshot::LedgerSnapshot;
use soroban_sdk::{contracttype, testutils::EnvTestConfig, Address, Env, IntoVal, TryFromVal, Val};

const DEFAULT_RPC_URL: &str = "https://mainnet.sorobanrpc.com";
const DEFAULT_OUTPUT_DIR: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp";
const NETWORK_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const POOL_ID: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
const XLM_ID: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const USDC_ID: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const USDC_ISSUER_ID: &str = "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN";
const PLANE_ID: &str = "CCABO2IQYDWRGGQ4DYQ73CV3ZFDBRZTEQNDDJMFT7JZO54CLS4RYJROY";
const POOL_WASM_SHA256: [u8; 32] = [
    0xae, 0x0d, 0xa5, 0xa8, 0x4b, 0x15, 0x80, 0x5c, 0x5c, 0x79, 0x31, 0xac, 0x56, 0x7a, 0x8d, 0x1b,
    0x34, 0xbe, 0x3f, 0x26, 0xb4, 0x83, 0x99, 0x3d, 0x9f, 0xf8, 0x0c, 0xb2, 0xc3, 0xde, 0x98, 0x52,
];
const MAX_COHERENCE_ATTEMPTS: usize = 12;

#[contracttype(export = false)]
#[derive(Clone)]
enum SacDataKey {
    Balance(Address),
}

#[contracttype(export = false)]
#[derive(Clone)]
enum PlaneDataKey {
    PoolData(Address),
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::parse()?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();

    let network: NetworkResult = rpc(
        &agent,
        &options.rpc_url,
        "getNetwork",
        &serde_json::json!({}),
    )?;
    ensure(
        network.passphrase == NETWORK_PASSPHRASE,
        format!("unexpected network passphrase: {}", network.passphrase),
    )?;
    ensure(
        network.protocol_version == SUPPORTED_PROTOCOL_VERSION,
        format!(
            "network protocol {} is not pinned Host protocol {}",
            network.protocol_version, SUPPORTED_PROTOCOL_VERSION
        ),
    )?;

    let plane_wasm_sha256 = discover_contract_wasm_hash(&agent, &options.rpc_url, PLANE_ID)?;
    let keys = scenario_keys(plane_wasm_sha256)?;
    let capture = capture_coherent_batch(&agent, &options.rpc_url, &keys)?;
    let state_archival = state_archival_settings(&capture.entries, &keys)?;
    let header = decode_header(&capture.latest.header_xdr)?;
    validate_header(&capture.latest, &header)?;

    let pool_wasm_path = options.output_dir.join("pool.wasm");
    let pool_wasm = fs::read(&pool_wasm_path).map_err(|error| {
        CaptureError(format!(
            "read pinned pool WASM {}: {error}",
            pool_wasm_path.display()
        ))
    })?;
    ensure(
        <[u8; 32]>::from(Sha256::digest(&pool_wasm)) == POOL_WASM_SHA256,
        "pinned pool.wasm SHA-256 mismatch",
    )?;
    validate_captured_entries(&capture.entries, &keys, &pool_wasm, plane_wasm_sha256)?;

    let network_id: [u8; 32] = Sha256::digest(NETWORK_PASSPHRASE.as_bytes()).into();
    let snapshot_entries = keys
        .iter()
        .filter(|key| key.include_in_snapshot)
        .map(|key| {
            let entry = capture.entries.get(&key.encoded).ok_or_else(|| {
                CaptureError(format!("final batch omitted required key {}", key.label))
            })?;
            Ok((
                Box::new(entry.key.clone()),
                (Box::new(entry.entry.clone()), entry.live_until_ledger_seq),
            ))
        })
        .collect::<Result<Vec<_>, CaptureError>>()?;
    let snapshot = LedgerSnapshot {
        protocol_version: capture.latest.protocol_version,
        sequence_number: capture.latest.sequence,
        timestamp: header.scp_value.close_time.0,
        network_id,
        base_reserve: header.base_reserve,
        min_persistent_entry_ttl: state_archival.min_persistent_ttl,
        min_temp_entry_ttl: state_archival.min_temporary_ttl,
        max_entry_ttl: state_archival.max_entry_ttl,
        ledger_entries: snapshot_entries,
    };

    fs::create_dir_all(&options.output_dir)?;
    let ledger_path = options.output_dir.join("ledger.json");
    snapshot.write_file(&ledger_path)?;
    let ledger_sha256 = sha256_file(&ledger_path)?;
    let snapshot_digest = canonical_ledger_digest(&snapshot)?;

    let manifest = Manifest {
        schema_version: 1,
        source_rpc: options.rpc_url,
        network_passphrase: NETWORK_PASSPHRASE,
        network_id_sha256: encode_hex(network_id),
        protocol_version: capture.latest.protocol_version,
        ledger_sequence: capture.latest.sequence,
        ledger_hash: capture.latest.id,
        ledger_close_time: header.scp_value.close_time.0,
        base_reserve: header.base_reserve,
        state_archival: StateArchivalManifest::from(state_archival),
        scenario: ScenarioManifest {
            pool: POOL_ID,
            xlm_sac: XLM_ID,
            usdc_sac: USDC_ID,
            usdc_issuer: USDC_ISSUER_ID,
            plane: PLANE_ID,
            plane_wasm_sha256: encode_hex(plane_wasm_sha256),
        },
        files: FilesManifest {
            ledger_json_sha256: encode_hex(ledger_sha256),
            pool_wasm_sha256: encode_hex(POOL_WASM_SHA256),
            canonical_ledger_digest: encode_hex(snapshot_digest),
        },
        entries: keys
            .iter()
            .map(|key| {
                let entry = &capture.entries[&key.encoded];
                EntryManifest {
                    label: key.label,
                    ledger_key_xdr: key.encoded.clone(),
                    last_modified_ledger: entry.entry.last_modified_ledger_seq,
                    live_until_ledger: entry.live_until_ledger_seq,
                    included_in_ledger_snapshot: key.include_in_snapshot,
                }
            })
            .collect(),
    };
    let manifest_path = options.output_dir.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    println!("captured ledger {}", capture.latest.sequence);
    println!("ledger snapshot: {}", ledger_path.display());
    println!("manifest: {}", manifest_path.display());
    println!("canonical digest: {}", encode_hex(snapshot_digest));
    Ok(())
}

fn capture_coherent_batch(
    agent: &ureq::Agent,
    rpc_url: &str,
    keys: &[ScenarioKey],
) -> Result<Capture, CaptureError> {
    let encoded_keys: Vec<&str> = keys.iter().map(|key| key.encoded.as_str()).collect();
    for attempt in 1..=MAX_COHERENCE_ATTEMPTS {
        let latest: LatestLedgerResult =
            rpc(agent, rpc_url, "getLatestLedger", &serde_json::json!({}))?;
        ensure(
            latest.protocol_version == SUPPORTED_PROTOCOL_VERSION,
            format!(
                "latest ledger protocol {} is not pinned Host protocol {}",
                latest.protocol_version, SUPPORTED_PROTOCOL_VERSION
            ),
        )?;
        let batch: LedgerEntriesResult = rpc(
            agent,
            rpc_url,
            "getLedgerEntries",
            &serde_json::json!({ "keys": encoded_keys }),
        )?;
        if batch.latest_ledger != latest.sequence {
            eprintln!(
                "coherence retry {attempt}/{MAX_COHERENCE_ATTEMPTS}: header ledger {}, entry batch ledger {}",
                latest.sequence, batch.latest_ledger
            );
            continue;
        }

        let entries = decode_entries(batch.entries.unwrap_or_default())?;
        ensure(
            entries.len() == keys.len(),
            format!(
                "coherent final batch returned {} of {} required entries",
                entries.len(),
                keys.len()
            ),
        )?;
        for key in keys {
            ensure(
                entries.contains_key(&key.encoded),
                format!("coherent final batch omitted {}", key.label),
            )?;
        }
        return Ok(Capture { latest, entries });
    }

    Err(CaptureError(format!(
        "could not obtain one coherent header/entry batch in {MAX_COHERENCE_ATTEMPTS} attempts"
    )))
}

fn discover_contract_wasm_hash(
    agent: &ureq::Agent,
    rpc_url: &str,
    contract_id: &str,
) -> Result<[u8; 32], CaptureError> {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    let key = contract_instance_key(sc_address(&env, contract_id));
    let encoded = encode_xdr(&key)?;
    let batch: LedgerEntriesResult = rpc(
        agent,
        rpc_url,
        "getLedgerEntries",
        &serde_json::json!({ "keys": [encoded] }),
    )?;
    let entries = decode_entries(batch.entries.unwrap_or_default())?;
    ensure(
        entries.len() == 1,
        format!("discovery did not return instance for {contract_id}"),
    )?;
    match &entries.values().next().unwrap().entry.data {
        LedgerEntryData::ContractData(data) => match &data.val {
            ScVal::ContractInstance(instance) => match &instance.executable {
                ContractExecutable::Wasm(Hash(hash)) => Ok(*hash),
                ContractExecutable::StellarAsset => Err(CaptureError(format!(
                    "discovered contract {contract_id} is not WASM"
                ))),
            },
            _ => Err(CaptureError(format!(
                "discovered contract {contract_id} has malformed instance"
            ))),
        },
        _ => Err(CaptureError(format!(
            "discovered contract {contract_id} returned wrong entry type"
        ))),
    }
}

fn scenario_keys(plane_wasm_sha256: [u8; 32]) -> Result<Vec<ScenarioKey>, CaptureError> {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    let pool = sc_address(&env, POOL_ID);
    let xlm = sc_address(&env, XLM_ID);
    let usdc = sc_address(&env, USDC_ID);
    let plane = sc_address(&env, PLANE_ID);
    let ScAddress::Account(issuer) = sc_address(&env, USDC_ISSUER_ID) else {
        return Err(CaptureError("USDC issuer is not an account".to_string()));
    };

    let keys = vec![
        ("pool_instance", contract_instance_key(pool.clone()), true),
        (
            "pool_code",
            LedgerKey::ContractCode(LedgerKeyContractCode {
                hash: Hash(POOL_WASM_SHA256),
            }),
            true,
        ),
        ("plane_instance", contract_instance_key(plane.clone()), true),
        (
            "plane_code",
            LedgerKey::ContractCode(LedgerKeyContractCode {
                hash: Hash(plane_wasm_sha256),
            }),
            true,
        ),
        (
            "plane_pool_data",
            plane_pool_data_key(&env, plane, &pool)?,
            true,
        ),
        ("xlm_sac_instance", contract_instance_key(xlm.clone()), true),
        (
            "usdc_sac_instance",
            contract_instance_key(usdc.clone()),
            true,
        ),
        ("xlm_balance_pool", sac_balance_key(&env, xlm, &pool)?, true),
        (
            "usdc_balance_pool",
            sac_balance_key(&env, usdc, &pool)?,
            true,
        ),
        (
            "usdc_issuer_account",
            LedgerKey::Account(LedgerKeyAccount { account_id: issuer }),
            true,
        ),
        (
            "state_archival_config",
            LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
                config_setting_id: ConfigSettingId::StateArchival,
            }),
            false,
        ),
    ];

    keys.into_iter()
        .map(|(label, key, include_in_snapshot)| {
            Ok(ScenarioKey {
                label,
                encoded: encode_xdr(&key)?,
                include_in_snapshot,
            })
        })
        .collect()
}

fn contract_instance_key(contract: ScAddress) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract,
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    })
}

fn sac_balance_key(
    env: &Env,
    token: ScAddress,
    holder: &ScAddress,
) -> Result<LedgerKey, CaptureError> {
    let holder = Address::try_from_val(env, holder)
        .map_err(|_| CaptureError("convert balance holder address".to_string()))?;
    let value: Val = SacDataKey::Balance(holder).into_val(env);
    let key = ScVal::try_from_val(env, &value)
        .map_err(|_| CaptureError("convert SAC Balance key".to_string()))?;
    Ok(LedgerKey::ContractData(LedgerKeyContractData {
        contract: token,
        key,
        durability: ContractDataDurability::Persistent,
    }))
}

fn plane_pool_data_key(
    env: &Env,
    plane: ScAddress,
    pool: &ScAddress,
) -> Result<LedgerKey, CaptureError> {
    let pool = Address::try_from_val(env, pool)
        .map_err(|_| CaptureError("convert plane pool address".to_string()))?;
    let value: Val = PlaneDataKey::PoolData(pool).into_val(env);
    let key = ScVal::try_from_val(env, &value)
        .map_err(|_| CaptureError("convert plane PoolData key".to_string()))?;
    Ok(LedgerKey::ContractData(LedgerKeyContractData {
        contract: plane,
        key,
        durability: ContractDataDurability::Persistent,
    }))
}

fn sc_address(env: &Env, strkey: &str) -> ScAddress {
    (&Address::from_str(env, strkey)).into()
}

fn decode_entries(entries: Vec<EntryWire>) -> Result<BTreeMap<String, FetchedEntry>, CaptureError> {
    let mut decoded = BTreeMap::new();
    for wire in entries {
        let key = decode_xdr::<LedgerKey>(&wire.key)?;
        let entry_data = decode_xdr::<LedgerEntryData>(&wire.xdr)?;
        let entry = LedgerEntry {
            last_modified_ledger_seq: wire.last_modified_ledger_seq,
            data: entry_data,
            ext: LedgerEntryExt::V0,
        };
        ensure(
            entry.to_key() == key,
            "RPC returned mismatched ledger key and entry",
        )?;
        ensure(
            decoded
                .insert(
                    wire.key.clone(),
                    FetchedEntry {
                        key,
                        entry,
                        live_until_ledger_seq: wire.live_until_ledger_seq,
                    },
                )
                .is_none(),
            "RPC returned a duplicate ledger key",
        )?;
    }
    Ok(decoded)
}

fn state_archival_settings<'a>(
    entries: &'a BTreeMap<String, FetchedEntry>,
    keys: &[ScenarioKey],
) -> Result<&'a soroban_env_host::xdr::StateArchivalSettings, CaptureError> {
    let key = keys
        .iter()
        .find(|key| key.label == "state_archival_config")
        .ok_or_else(|| CaptureError("state archival key missing from capture plan".to_string()))?;
    match &entries[&key.encoded].entry.data {
        LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(settings)) => Ok(settings),
        _ => Err(CaptureError(
            "state archival key returned an unexpected entry type".to_string(),
        )),
    }
}

fn validate_captured_entries(
    entries: &BTreeMap<String, FetchedEntry>,
    keys: &[ScenarioKey],
    pool_wasm: &[u8],
    plane_wasm_sha256: [u8; 32],
) -> Result<(), CaptureError> {
    let by_label = |label: &str| -> Result<&FetchedEntry, CaptureError> {
        let key = keys
            .iter()
            .find(|key| key.label == label)
            .ok_or_else(|| CaptureError(format!("capture plan has no {label}")))?;
        entries
            .get(&key.encoded)
            .ok_or_else(|| CaptureError(format!("capture batch has no {label}")))
    };

    match &by_label("pool_instance")?.entry.data {
        LedgerEntryData::ContractData(data) => match &data.val {
            ScVal::ContractInstance(instance) => ensure(
                instance.executable == ContractExecutable::Wasm(Hash(POOL_WASM_SHA256)),
                "pool instance points to an unexpected code hash",
            )?,
            _ => return Err(CaptureError("pool instance value is malformed".to_string())),
        },
        _ => {
            return Err(CaptureError(
                "pool instance entry has wrong type".to_string(),
            ))
        }
    }
    match &by_label("pool_code")?.entry.data {
        LedgerEntryData::ContractCode(code) => ensure(
            code.hash == Hash(POOL_WASM_SHA256) && code.code.as_slice() == pool_wasm,
            "captured pool code does not match pinned pool.wasm",
        )?,
        _ => return Err(CaptureError("pool code entry has wrong type".to_string())),
    }
    match &by_label("plane_instance")?.entry.data {
        LedgerEntryData::ContractData(data) => match &data.val {
            ScVal::ContractInstance(instance) => ensure(
                instance.executable == ContractExecutable::Wasm(Hash(plane_wasm_sha256)),
                "plane changed code hash between discovery and final batch",
            )?,
            _ => {
                return Err(CaptureError(
                    "plane instance value is malformed".to_string(),
                ))
            }
        },
        _ => {
            return Err(CaptureError(
                "plane instance entry has wrong type".to_string(),
            ))
        }
    }
    match &by_label("plane_code")?.entry.data {
        LedgerEntryData::ContractCode(code) => ensure(
            code.hash == Hash(plane_wasm_sha256)
                && <[u8; 32]>::from(Sha256::digest(code.code.as_slice())) == plane_wasm_sha256,
            "captured plane code bytes do not match its instance hash",
        )?,
        _ => return Err(CaptureError("plane code entry has wrong type".to_string())),
    }
    ensure(
        matches!(
            by_label("plane_pool_data")?.entry.data,
            LedgerEntryData::ContractData(_)
        ),
        "plane pool data entry has wrong type",
    )?;
    for label in ["xlm_sac_instance", "usdc_sac_instance"] {
        match &by_label(label)?.entry.data {
            LedgerEntryData::ContractData(data) => match &data.val {
                ScVal::ContractInstance(instance) => ensure(
                    instance.executable == ContractExecutable::StellarAsset,
                    format!("{label} is not a Stellar Asset Contract"),
                )?,
                _ => return Err(CaptureError(format!("{label} value is malformed"))),
            },
            _ => return Err(CaptureError(format!("{label} entry has wrong type"))),
        }
    }
    ensure(
        matches!(
            by_label("usdc_issuer_account")?.entry.data,
            LedgerEntryData::Account(_)
        ),
        "USDC issuer entry has wrong type",
    )
}

fn decode_header(encoded: &str) -> Result<LedgerHeader, CaptureError> {
    decode_xdr(encoded)
}

fn validate_header(latest: &LatestLedgerResult, header: &LedgerHeader) -> Result<(), CaptureError> {
    ensure(
        header.ledger_seq == latest.sequence,
        "getLatestLedger header sequence mismatch",
    )?;
    ensure(
        header.scp_value.close_time.0
            == latest.close_time.parse::<u64>().map_err(|error| {
                CaptureError(format!("invalid getLatestLedger closeTime: {error}"))
            })?,
        "getLatestLedger closeTime/header mismatch",
    )?;
    let header_bytes = BASE64
        .decode(&latest.header_xdr)
        .map_err(|error| CaptureError(format!("decode ledger header: {error}")))?;
    ensure(
        encode_hex(Sha256::digest(header_bytes)) == latest.id,
        "getLatestLedger id/header hash mismatch",
    )
}

fn encode_xdr<T: WriteXdr>(value: &T) -> Result<String, CaptureError> {
    value
        .to_xdr(Limits::none())
        .map(|bytes| BASE64.encode(bytes))
        .map_err(|error| CaptureError(format!("encode XDR: {error}")))
}

fn decode_xdr<T: ReadXdr>(encoded: &str) -> Result<T, CaptureError> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|error| CaptureError(format!("decode base64 XDR: {error}")))?;
    T::from_xdr(bytes, Limits::none()).map_err(|error| CaptureError(format!("decode XDR: {error}")))
}

fn rpc<T: DeserializeOwned>(
    agent: &ureq::Agent,
    url: &str,
    method: &str,
    params: &serde_json::Value,
) -> Result<T, CaptureError> {
    let response = agent
        .post(url)
        .send_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        }))
        .map_err(|error| CaptureError(format!("RPC {method}: {error}")))?;
    let body: JsonRpcResponse<T> = response
        .into_json()
        .map_err(|error| CaptureError(format!("parse RPC {method}: {error}")))?;
    if let Some(error) = body.error {
        return Err(CaptureError(format!("RPC {method}: {error}")));
    }
    body.result
        .ok_or_else(|| CaptureError(format!("RPC {method} returned no result")))
}

fn sha256_file(path: &Path) -> Result<[u8; 32], CaptureError> {
    let bytes = fs::read(path)
        .map_err(|error| CaptureError(format!("read {}: {error}", path.display())))?;
    Ok(Sha256::digest(bytes).into())
}

fn encode_hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String is infallible");
    }
    encoded
}

fn ensure(condition: bool, message: impl Into<String>) -> Result<(), CaptureError> {
    if condition {
        Ok(())
    } else {
        Err(CaptureError(message.into()))
    }
}

#[derive(Debug)]
struct CaptureError(String);

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CaptureError {}

struct Options {
    rpc_url: String,
    output_dir: PathBuf,
}

impl Options {
    fn parse() -> Result<Self, CaptureError> {
        let mut rpc_url = DEFAULT_RPC_URL.to_string();
        let mut output_dir = PathBuf::from(DEFAULT_OUTPUT_DIR);
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--rpc-url" => {
                    rpc_url = args
                        .next()
                        .ok_or_else(|| CaptureError("--rpc-url needs a value".to_string()))?;
                }
                "--out-dir" => {
                    output_dir = PathBuf::from(
                        args.next()
                            .ok_or_else(|| CaptureError("--out-dir needs a value".to_string()))?,
                    );
                }
                _ => return Err(CaptureError(format!("unknown argument: {arg}"))),
            }
        }
        Ok(Self {
            rpc_url,
            output_dir,
        })
    }
}

struct ScenarioKey {
    label: &'static str,
    encoded: String,
    include_in_snapshot: bool,
}

struct FetchedEntry {
    key: LedgerKey,
    entry: LedgerEntry,
    live_until_ledger_seq: Option<u32>,
}

struct Capture {
    latest: LatestLedgerResult,
    entries: BTreeMap<String, FetchedEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NetworkResult {
    passphrase: String,
    protocol_version: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LatestLedgerResult {
    id: String,
    protocol_version: u32,
    sequence: u32,
    close_time: String,
    header_xdr: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerEntriesResult {
    entries: Option<Vec<EntryWire>>,
    latest_ledger: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryWire {
    key: String,
    xdr: String,
    last_modified_ledger_seq: u32,
    live_until_ledger_seq: Option<u32>,
}

#[derive(Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct Manifest<'a> {
    schema_version: u32,
    source_rpc: String,
    network_passphrase: &'a str,
    network_id_sha256: String,
    protocol_version: u32,
    ledger_sequence: u32,
    ledger_hash: String,
    ledger_close_time: u64,
    base_reserve: u32,
    state_archival: StateArchivalManifest,
    scenario: ScenarioManifest<'a>,
    files: FilesManifest,
    entries: Vec<EntryManifest<'a>>,
}

#[derive(Serialize)]
#[allow(clippy::struct_field_names)]
struct StateArchivalManifest {
    min_persistent_entry_ttl: u32,
    min_temporary_entry_ttl: u32,
    max_entry_ttl: u32,
}

impl From<&soroban_env_host::xdr::StateArchivalSettings> for StateArchivalManifest {
    fn from(settings: &soroban_env_host::xdr::StateArchivalSettings) -> Self {
        Self {
            min_persistent_entry_ttl: settings.min_persistent_ttl,
            min_temporary_entry_ttl: settings.min_temporary_ttl,
            max_entry_ttl: settings.max_entry_ttl,
        }
    }
}

#[derive(Serialize)]
struct ScenarioManifest<'a> {
    pool: &'a str,
    xlm_sac: &'a str,
    usdc_sac: &'a str,
    usdc_issuer: &'a str,
    plane: &'a str,
    plane_wasm_sha256: String,
}

#[derive(Serialize)]
struct FilesManifest {
    ledger_json_sha256: String,
    pool_wasm_sha256: String,
    canonical_ledger_digest: String,
}

#[derive(Serialize)]
struct EntryManifest<'a> {
    label: &'a str,
    ledger_key_xdr: String,
    last_modified_ledger: u32,
    live_until_ledger: Option<u32>,
    included_in_ledger_snapshot: bool,
}
