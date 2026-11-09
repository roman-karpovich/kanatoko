#![no_std]

use soroban_sdk::{contract, contractclient, contractimpl, symbol_short, Address, Env, Vec};

#[contractclient(name = "RouterClient")]
pub trait Router {
    fn router_get_amounts_out(env: Env, amount_in: i128, path: Vec<Address>) -> Vec<i128>;
    fn swap_exact_tokens_for_tokens(
        env: Env,
        amount_in: i128,
        amount_out_min: i128,
        path: Vec<Address>,
        to: Address,
        deadline: u64,
    ) -> Vec<i128>;
}

#[contractclient(name = "PairClient")]
pub trait Pair {
    fn token_0(env: Env) -> Address;
    fn token_1(env: Env) -> Address;
    fn get_reserves(env: Env) -> (i128, i128);
}

#[contract]
pub struct SoroswapWrapper;

#[contractimpl]
impl SoroswapWrapper {
    pub fn __constructor(env: Env, router: Address, pair: Address) {
        env.storage()
            .instance()
            .set(&symbol_short!("router"), &router);
        env.storage().instance().set(&symbol_short!("pair"), &pair);
    }

    pub fn token_0(env: Env) -> Address {
        let pair = pair(&env);
        PairClient::new(&env, &pair).token_0()
    }

    pub fn token_1(env: Env) -> Address {
        let pair = pair(&env);
        PairClient::new(&env, &pair).token_1()
    }

    pub fn get_reserves(env: Env) -> (i128, i128) {
        let pair = pair(&env);
        PairClient::new(&env, &pair).get_reserves()
    }

    pub fn router_get_amounts_out(env: Env, amount_in: i128, path: Vec<Address>) -> Vec<i128> {
        let router = router(&env);
        RouterClient::new(&env, &router).router_get_amounts_out(&amount_in, &path)
    }

    pub fn swap_exact_tokens_for_tokens(
        env: Env,
        amount_in: i128,
        amount_out_min: i128,
        path: Vec<Address>,
        to: Address,
        deadline: u64,
    ) -> Vec<i128> {
        to.require_auth();
        let router = router(&env);
        RouterClient::new(&env, &router).swap_exact_tokens_for_tokens(
            &amount_in,
            &amount_out_min,
            &path,
            &to,
            &deadline,
        )
    }
}

fn router(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&symbol_short!("router"))
        .expect("wrapper must be initialized by its constructor")
}

fn pair(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&symbol_short!("pair"))
        .expect("wrapper must be initialized by its constructor")
}
