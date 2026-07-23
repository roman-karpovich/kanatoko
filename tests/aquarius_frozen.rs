use kanatoko::{Fork, FrozenFixture};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{ContractEventBody, Hash, ScAddress, ScSymbol, ScVal};
use soroban_sdk::{
    testutils::{
        AuthorizedFunction, AuthorizedInvocation, EventSnapshot, MockAuth, MockAuthInvoke,
    },
    token::{StellarAssetClient, TokenClient},
    Address, IntoVal, Symbol, TryFromVal,
};

mod pool {
    #![allow(clippy::ref_option, clippy::too_many_arguments)]

    soroban_sdk::contractimport!(
        file = "fixtures/mainnet/aquarius-xlm-usdc-cp/pool.wasm",
        sha256 = "ae0da5a84b15805c5c7931ac567a8d1b34be3f26b483993d9ff80cb2c3de9852",
    );
}

const NETWORK_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
const POOL_ID: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
const XLM_ID: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const USDC_ID: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const USDC_ADMIN_ID: &str = "CCPLJV7AKKFIE4LXFVUWZXDI2HNLEC7U3CQHAMVDLYHQFGMGVTCR4D5W";
const ONE_USDC: u128 = 10_000_000;
const POOL_WASM: &[u8] = include_bytes!("../fixtures/mainnet/aquarius-xlm-usdc-cp/pool.wasm");
const POOL_WASM_SHA256: [u8; 32] = [
    0xae, 0x0d, 0xa5, 0xa8, 0x4b, 0x15, 0x80, 0x5c, 0x5c, 0x79, 0x31, 0xac, 0x56, 0x7a, 0x8d, 0x1b,
    0x34, 0xbe, 0x3f, 0x26, 0xb4, 0x83, 0x99, 0x3d, 0x9f, 0xf8, 0x0c, 0xb2, 0xc3, 0xde, 0x98, 0x52,
];
const FIXTURE_LEDGER_DIGEST: [u8; 32] = [
    0xcf, 0x3c, 0xa3, 0x24, 0x79, 0x27, 0xda, 0x7c, 0x7a, 0xde, 0x18, 0xa7, 0x34, 0xac, 0xf4, 0x16,
    0xf8, 0x7c, 0x9a, 0xd1, 0x50, 0x9a, 0x8f, 0xa3, 0xa0, 0x03, 0xa1, 0x9d, 0x7c, 0x0d, 0x9b, 0x9d,
];

#[test]
#[allow(clippy::too_many_lines)]
fn frozen_mainnet_quote_swap_requote_changes_local_price() {
    assert_eq!(
        <[u8; 32]>::from(Sha256::digest(POOL_WASM)),
        POOL_WASM_SHA256
    );
    let fixture = FrozenFixture::from_file(
        "fixtures/mainnet/aquarius-xlm-usdc-cp/ledger.json",
        NETWORK_PASSPHRASE,
    )
    .unwrap();
    assert_eq!(fixture.ledger_digest(), FIXTURE_LEDGER_DIGEST);
    assert_eq!(fixture.ledger_snapshot().sequence_number, 63_599_433);
    let fork = Fork::from_fixture(&fixture);
    let env = fork.env();

    let pool_address = Address::from_str(env, POOL_ID);
    let xlm = Address::from_str(env, XLM_ID);
    let usdc = Address::from_str(env, USDC_ID);
    let pool = pool::Client::new(env, &pool_address);
    let xlm_token = TokenClient::new(env, &xlm);
    let usdc_token = TokenClient::new(env, &usdc);
    let usdc_admin = StellarAssetClient::new(env, &usdc);

    assert_eq!(
        pool.get_tokens(),
        soroban_sdk::vec![env, xlm.clone(), usdc.clone()]
    );
    assert_eq!(xlm_token.decimals(), 7);
    assert_eq!(usdc_token.decimals(), 7);
    assert_eq!(usdc_admin.admin(), Address::from_str(env, USDC_ADMIN_ID));

    let quote_before = pool.estimate_swap(&1, &0, &ONE_USDC);
    assert!(quote_before > 0);
    let reserves_before = pool.get_reserves();
    assert_eq!(reserves_before.len(), 2);
    assert!(reserves_before.get(0).unwrap() > 0);
    assert!(reserves_before.get(1).unwrap() > 0);
    let digest_after_quote = fork.ledger_digest().unwrap();

    let user = deterministic_contract_user(env);
    let minted = reserves_before.get(1).unwrap() / 10;
    let minted_i128 = i128::try_from(minted).unwrap();
    assert!(minted > ONE_USDC);

    // The live USDC admin is itself a contract. `MockAuth` would register a
    // test auth contract at that address and replace imported mainnet state,
    // so discover this one tree in recording mode, assert it exactly, then
    // disable recording before installing the exact synthetic-user swap auth.
    env.mock_all_auths();
    usdc_admin.mint(&user, &minted_i128);
    let mint_auth = env.auths();
    assert_eq!(
        mint_auth,
        [(
            Address::from_str(env, USDC_ADMIN_ID),
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
    assert_eq!(xlm_token.balance(&user), 0);
    assert_ne!(fork.ledger_digest().unwrap(), digest_after_quote);
    env.set_auths(&[]);

    let user_usdc_before_swap = usdc_token.balance(&user);
    let user_xlm_before_swap = xlm_token.balance(&user);
    let pool_usdc_before_swap = usdc_token.balance(&pool_address);
    let pool_xlm_before_swap = xlm_token.balance(&pool_address);
    let digest_before_swap = fork.ledger_digest().unwrap();
    let events_before_swap = env.to_snapshot().events.0.len();

    let transfer_auth = MockAuthInvoke {
        contract: &usdc,
        fn_name: "transfer",
        args: (user.clone(), pool_address.clone(), minted_i128).into_val(env),
        sub_invokes: &[],
    };
    let swap_sub_invokes = [transfer_auth];
    let swap_auth = MockAuthInvoke {
        contract: &pool_address,
        fn_name: "swap",
        args: (user.clone(), 1_u32, 0_u32, minted, 0_u128).into_val(env),
        sub_invokes: &swap_sub_invokes,
    };
    let mock_auth = MockAuth {
        address: &user,
        invoke: &swap_auth,
    };
    let received_xlm = pool
        .mock_auths(&[mock_auth])
        .swap(&user, &1, &0, &minted, &0);
    assert!(received_xlm > quote_before);
    let swap_events = env.to_snapshot().events.0;
    assert_pool_swap_events(&swap_events[events_before_swap..], &pool_address);

    let observed_swap_auth = env.auths();
    assert_eq!(
        observed_swap_auth,
        [(
            user.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    pool_address.clone(),
                    Symbol::new(env, "swap"),
                    (user.clone(), 1_u32, 0_u32, minted, 0_u128).into_val(env),
                )),
                sub_invocations: std::vec![AuthorizedInvocation {
                    function: AuthorizedFunction::Contract((
                        usdc.clone(),
                        Symbol::new(env, "transfer"),
                        (user.clone(), pool_address.clone(), minted_i128).into_val(env),
                    )),
                    sub_invocations: std::vec![],
                }],
            },
        )]
    );
    assert_eq!(
        usdc_token.balance(&user),
        user_usdc_before_swap - minted_i128
    );
    assert_eq!(
        xlm_token.balance(&user),
        user_xlm_before_swap + i128::try_from(received_xlm).unwrap()
    );
    assert_eq!(
        usdc_token.balance(&pool_address),
        pool_usdc_before_swap + minted_i128
    );
    assert_eq!(
        xlm_token.balance(&pool_address),
        pool_xlm_before_swap - i128::try_from(received_xlm).unwrap()
    );

    let reserves_after = pool.get_reserves();
    assert_eq!(
        reserves_after.get(0).unwrap(),
        reserves_before.get(0).unwrap() - received_xlm
    );
    assert!(reserves_after.get(1).unwrap() > reserves_before.get(1).unwrap());
    assert!(reserves_after.get(1).unwrap() <= reserves_before.get(1).unwrap() + minted);
    assert_ne!(fork.ledger_digest().unwrap(), digest_before_swap);

    let quote_after = pool.estimate_swap(&1, &0, &ONE_USDC);
    assert_ne!(quote_after, quote_before);
    assert!(quote_after < quote_before);
}

fn deterministic_contract_user(env: &soroban_sdk::Env) -> Address {
    Address::try_from_val(env, &ScAddress::Contract(Hash([0x4b; 32]).into())).unwrap()
}

fn assert_pool_swap_events(events: &[EventSnapshot], pool: &Address) {
    let pool_id = match ScAddress::from(pool) {
        ScAddress::Contract(contract_id) => Some(contract_id),
        _ => panic!("Aquarius pool must be a contract address"),
    };
    let names: std::vec::Vec<_> = events
        .iter()
        .filter(|event| !event.failed_call && event.event.contract_id == pool_id)
        .map(|event| match &event.event.body {
            ContractEventBody::V0(body) => body.topics.first().cloned().unwrap(),
        })
        .collect();
    assert_eq!(
        names,
        [
            ScVal::Symbol(ScSymbol("trade".try_into().unwrap())),
            ScVal::Symbol(ScSymbol("update_reserves".try_into().unwrap())),
        ]
    );
}
