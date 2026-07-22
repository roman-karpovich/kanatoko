#![no_std]

use soroban_sdk::{contract, contractclient, contractimpl, symbol_short, Address, Env, Vec};

#[contractclient(name = "AquariusPoolClient")]
pub trait AquariusPool {
    fn get_tokens(env: Env) -> Vec<Address>;
    fn get_reserves(env: Env) -> Vec<u128>;
    fn estimate_swap(env: Env, in_idx: u32, out_idx: u32, in_amount: u128) -> u128;
    fn swap(
        env: Env,
        user: Address,
        in_idx: u32,
        out_idx: u32,
        in_amount: u128,
        out_min: u128,
    ) -> u128;
}

#[contract]
pub struct AquariusWrapper;

#[contractimpl]
impl AquariusWrapper {
    pub fn __constructor(env: Env, pool: Address) {
        env.storage().instance().set(&symbol_short!("pool"), &pool);
    }

    pub fn get_tokens(env: Env) -> Vec<Address> {
        let pool = pool(&env);
        AquariusPoolClient::new(&env, &pool).get_tokens()
    }

    pub fn get_reserves(env: Env) -> Vec<u128> {
        let pool = pool(&env);
        AquariusPoolClient::new(&env, &pool).get_reserves()
    }

    pub fn estimate_swap(env: Env, in_idx: u32, out_idx: u32, in_amount: u128) -> u128 {
        let pool = pool(&env);
        AquariusPoolClient::new(&env, &pool).estimate_swap(&in_idx, &out_idx, &in_amount)
    }

    pub fn swap(
        env: Env,
        user: Address,
        in_idx: u32,
        out_idx: u32,
        in_amount: u128,
        out_min: u128,
    ) -> u128 {
        user.require_auth();
        let pool = pool(&env);
        AquariusPoolClient::new(&env, &pool).swap(&user, &in_idx, &out_idx, &in_amount, &out_min)
    }
}

fn pool(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&symbol_short!("pool"))
        .expect("wrapper must be initialized by its constructor")
}
