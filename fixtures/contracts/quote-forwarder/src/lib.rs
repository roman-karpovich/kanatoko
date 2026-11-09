#![no_std]

use soroban_sdk::{contract, contractclient, contractimpl, symbol_short, Address, Env};

#[contractclient(name = "QuoteClient")]
pub trait Quote {
    fn estimate_swap(env: Env, in_idx: u32, out_idx: u32, in_amount: u128) -> u128;
}

#[contract]
pub struct QuoteForwarder;

#[contractimpl]
impl QuoteForwarder {
    pub fn __constructor(env: Env, target: Address) {
        env.storage()
            .instance()
            .set(&symbol_short!("target"), &target);
    }

    pub fn estimate_swap(env: Env, in_idx: u32, out_idx: u32, in_amount: u128) -> u128 {
        let target: Address = env
            .storage()
            .instance()
            .get(&symbol_short!("target"))
            .expect("forwarder must be initialized by its constructor");
        QuoteClient::new(&env, &target).estimate_swap(&in_idx, &out_idx, &in_amount)
    }
}
