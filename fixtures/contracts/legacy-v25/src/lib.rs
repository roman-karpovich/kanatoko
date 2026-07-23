#![no_std]

use soroban_sdk::{contract, contractimpl};

#[contract]
pub struct LegacyV25Fixture;

#[contractimpl]
impl LegacyV25Fixture {
    pub const fn sdk_major() -> u32 {
        25
    }

    pub const fn add(left: i64, right: i64) -> i64 {
        left + right
    }
}
