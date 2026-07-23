//! Capture or replay the Aquarius XLM/USDC constant-product scenario.
//!
//! Capture starts with only an RPC URL. The scenario names the pool like every
//! other address; contract code and every touched ledger key are discovered by
//! `CaptureBuilder`.
//! The local `pool.wasm` is used only by `contractimport!` to generate the typed
//! client ABI. Replay reads one validated bundle and performs no RPC calls.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use kanatoko::{CaptureBuilder, CapturedFixture};
use soroban_env_host::xdr::{Hash, ScAddress};
use soroban_sdk::{
    testutils::{AuthorizedFunction, AuthorizedInvocation, MockAuth, MockAuthInvoke},
    token::{StellarAssetClient, TokenClient},
    Address, Env, IntoVal, Symbol, TryFromVal,
};

mod pool {
    #![allow(clippy::ref_option, clippy::too_many_arguments)]

    soroban_sdk::contractimport!(
        file = "fixtures/mainnet/aquarius-xlm-usdc-cp/pool.wasm",
        sha256 = "ae0da5a84b15805c5c7931ac567a8d1b34be3f26b483993d9ff80cb2c3de9852",
    );
}

const DEFAULT_RPC_URL: &str = "https://mainnet.sorobanrpc.com";
const DEFAULT_OUTPUT_DIR: &str = "fixtures/mainnet/aquarius-xlm-usdc-cp";
const DEFAULT_POOL_ID: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
const MAINNET_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";

fn main() -> Result<(), Box<dyn Error>> {
    match Options::parse()?.mode {
        Mode::Capture {
            rpc_url,
            bundle_path,
        } => capture(&rpc_url, &bundle_path),
        Mode::Replay { bundle_path } => replay(&bundle_path),
    }
}

pub(crate) fn capture(rpc_url: &str, bundle_path: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = bundle_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let captured = CaptureBuilder::mainnet(rpc_url)?.capture(aquarius_scenario)?;
    captured.write_file(bundle_path)?;

    // Read back through the public fail-closed loader and prove that the exact
    // saved artifact, rather than the in-memory capture, replays offline.
    let loaded = CapturedFixture::from_file(bundle_path, MAINNET_PASSPHRASE)?;
    loaded.replay(aquarius_scenario)?;

    println!("captured ledger {}", loaded.provenance().ledger_sequence());
    println!("source origin: {}", loaded.provenance().rpc_origin());
    println!("scenario pool: {DEFAULT_POOL_ID}");
    println!("present entries: {}", loaded.report().present_entries());
    println!(
        "confirmed absent entries: {}",
        loaded.report().absent_entries()
    );
    println!("bundle: {}", bundle_path.display());
    println!("offline replay: ok");
    Ok(())
}

fn replay(bundle_path: &Path) -> Result<(), Box<dyn Error>> {
    let captured = CapturedFixture::from_file(bundle_path, MAINNET_PASSPHRASE)?;
    captured.replay(aquarius_scenario)?;
    println!(
        "replayed ledger {}",
        captured.provenance().ledger_sequence()
    );
    println!("scenario pool: {DEFAULT_POOL_ID}");
    println!("bundle: {}", bundle_path.display());
    println!("offline replay: ok");
    Ok(())
}

fn aquarius_scenario(env: &Env) {
    let pool_id = Address::from_str(env, DEFAULT_POOL_ID);
    let pool = pool::Client::new(env, &pool_id);
    let tokens = pool.get_tokens();
    assert_eq!(tokens.len(), 2);
    let usdc = tokens.get(1).expect("pool must have a token at index 1");
    let usdc_token = TokenClient::new(env, &usdc);
    let usdc_admin = StellarAssetClient::new(env, &usdc);
    let one_usdc = 10_u128
        .checked_pow(usdc_token.decimals())
        .expect("token decimals must fit u128");

    let quote_before = pool.estimate_swap(&1, &0, &one_usdc);
    assert!(quote_before > 0);
    let reserves_before = pool.get_reserves();
    assert_eq!(reserves_before.len(), 2);
    let minted = reserves_before.get(1).expect("USDC reserve must exist") / 10;
    assert!(minted > one_usdc);
    let minted_i128 = i128::try_from(minted).expect("10% of reserve must fit i128");
    let user = deterministic_contract_user(env);

    // The live SAC admin can itself be a contract. Recording mode observes the
    // real admin authorization without replacing imported contract state.
    let admin = usdc_admin.admin();
    env.mock_all_auths();
    usdc_admin.mint(&user, &minted_i128);
    assert_eq!(
        env.auths(),
        [(
            admin,
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    usdc.clone(),
                    Symbol::new(env, "mint"),
                    (user.clone(), minted_i128).into_val(env),
                )),
                sub_invocations: std::vec![],
            },
        )]
    );
    assert_eq!(usdc_token.balance(&user), minted_i128);
    env.set_auths(&[]);

    let transfer = MockAuthInvoke {
        contract: &usdc,
        fn_name: "transfer",
        args: (user.clone(), pool_id.clone(), minted_i128).into_val(env),
        sub_invokes: &[],
    };
    let nested = [transfer];
    let swap = MockAuthInvoke {
        contract: &pool_id,
        fn_name: "swap",
        args: (user.clone(), 1_u32, 0_u32, minted, 0_u128).into_val(env),
        sub_invokes: &nested,
    };
    let received = pool
        .mock_auths(&[MockAuth {
            address: &user,
            invoke: &swap,
        }])
        .swap(&user, &1, &0, &minted, &0);
    assert!(received > 0);
    assert_eq!(
        env.auths(),
        [(
            user.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    pool_id.clone(),
                    Symbol::new(env, "swap"),
                    (user.clone(), 1_u32, 0_u32, minted, 0_u128).into_val(env),
                )),
                sub_invocations: std::vec![AuthorizedInvocation {
                    function: AuthorizedFunction::Contract((
                        usdc,
                        Symbol::new(env, "transfer"),
                        (user, pool_id.clone(), minted_i128).into_val(env),
                    )),
                    sub_invocations: std::vec![],
                }],
            },
        )]
    );

    let quote_after = pool.estimate_swap(&1, &0, &one_usdc);
    assert_ne!(quote_after, quote_before);
    assert!(quote_after < quote_before);
}

fn deterministic_contract_user(env: &Env) -> Address {
    Address::try_from_val(env, &ScAddress::Contract(Hash([0x4b; 32]).into()))
        .expect("synthetic contract user must be a valid Address")
}

struct Options {
    mode: Mode,
}

enum Mode {
    Capture {
        rpc_url: String,
        bundle_path: PathBuf,
    },
    Replay {
        bundle_path: PathBuf,
    },
}

impl Options {
    fn parse() -> Result<Self, OptionsError> {
        let mut rpc_url = DEFAULT_RPC_URL.to_string();
        let mut bundle_path = PathBuf::from(DEFAULT_OUTPUT_DIR).join("capture.json");
        let mut replay_path = None;
        let mut args = std::env::args().skip(1);
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--rpc-url" => rpc_url = next_value(&mut args, "--rpc-url")?,
                "--bundle" => bundle_path = PathBuf::from(next_value(&mut args, "--bundle")?),
                "--out-dir" => {
                    bundle_path =
                        PathBuf::from(next_value(&mut args, "--out-dir")?).join("capture.json");
                }
                "--replay" => {
                    replay_path = Some(PathBuf::from(next_value(&mut args, "--replay")?));
                }
                _ => return Err(OptionsError(format!("unknown argument: {argument}"))),
            }
        }
        let mode = replay_path.map_or(
            Mode::Capture {
                rpc_url,
                bundle_path,
            },
            |bundle_path| Mode::Replay { bundle_path },
        );
        Ok(Self { mode })
    }
}

fn next_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, OptionsError> {
    arguments
        .next()
        .ok_or_else(|| OptionsError(format!("{option} needs a value")))
}

#[derive(Debug)]
struct OptionsError(String);

impl std::fmt::Display for OptionsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for OptionsError {}
