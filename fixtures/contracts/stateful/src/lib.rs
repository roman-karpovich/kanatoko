#![no_std]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, symbol_short, Address, Env,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum FixtureError {
    Rejected = 1,
}

#[contractevent(topics = ["increment"])]
pub struct Incremented {
    pub value: i64,
}

#[contractevent(topics = ["reject"])]
pub struct Rejected {
    pub value: i64,
}

#[contract]
pub struct StatefulFixture;

#[contractimpl]
impl StatefulFixture {
    pub fn __constructor(env: Env, initial: i64) {
        env.storage()
            .persistent()
            .set(&symbol_short!("value"), &initial);
    }

    pub fn get(env: Env) -> i64 {
        env.storage()
            .persistent()
            .get(&symbol_short!("value"))
            .unwrap_or(0)
    }

    pub fn increment(env: Env, amount: i64) -> i64 {
        let next = Self::get(env.clone()) + amount;
        env.storage()
            .persistent()
            .set(&symbol_short!("value"), &next);
        Incremented { value: next }.publish(&env);
        next
    }

    pub fn authorized_increment(env: Env, user: Address, amount: i64) -> i64 {
        user.require_auth();
        Self::increment(env, amount)
    }

    pub fn increment_then_fail(env: Env, amount: i64) -> Result<i64, FixtureError> {
        let next = Self::increment(env.clone(), amount);
        Rejected { value: next }.publish(&env);
        Err(FixtureError::Rejected)
    }
}
