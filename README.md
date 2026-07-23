# Kanatoko

**Test Soroban contracts against real Stellar state without deploying or
spending funds.**

Write one stateful Rust test. Kanatoko runs it to discover every contract,
network WASM, and ledger entry it touches, freezes one coherent ledger, then
runs the same test again in strict mode with zero RPC fallback.

No manual capture script. No duplicated scenario. No fake implementation of
the contracts under test.

```toml
[dev-dependencies]
kanatoko = { git = "https://github.com/roman-karpovich/kanatoko", features = ["capture"] }
soroban-sdk = { version = "=27.0.0", features = ["testutils"] }
```

## Example: move a real Aquarius pool price

```rust,ignore
use kanatoko::mainnet;
use soroban_sdk::Address;

mod pool_abi {
    // Any ABI-compatible build is enough; this file is never executed.
    soroban_sdk::contractimport!(file = "tests/wasm/aquarius_pool_abi.wasm");
}

const POOL: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const USER: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";

#[test]
fn swap_moves_the_real_pool_price() {
    mainnet(POOL)
        .cache(".kanatoko/aquarius-swap.json")
        .run(|fork| {
            let env = fork.env();
            let pool_id = fork.contract(POOL);
            let usdc = fork.contract(USDC);
            let user = fork.contract(USER);

            // Typed calls and dynamic calls share this Env and its mutations.
            let pool = pool_abi::Client::new(env, &pool_id);
            let before = pool.estimate_swap(&1, &0, &10_000_000);
            let reserves = pool.get_reserves();
            let amount = reserves.get(1).unwrap() / 10;

            fork.mock_all_auths();
            let admin = fork.invoke::<Address>(&usdc, "admin", ());

            // SAC mint is (user, amount); admin belongs to the auth tree.
            fork.invoke::<()>(
                &usdc,
                "mint",
                (user.clone(), i128::try_from(amount).unwrap()),
            );
            assert_eq!(env.auths()[0].0, admin);

            let received = fork.invoke::<u128>(
                &pool_id,
                "swap",
                (user, 1_u32, 0_u32, amount, 0_u128),
            );
            assert!(received > 0);

            let after = pool.estimate_swap(&1, &0, &10_000_000);
            assert_ne!(after, before);
        })
        .unwrap();
}
```

On a cache miss, this single body drives dependency discovery and creates the
fixture. Later runs use the frozen state directly. Use `.refresh()` to capture
a newer mainnet ledger or `.offline()` to require a cache-only CI run. Keep one
cache path per scenario.

The body can run several times during discovery and strict replay, so it must
be deterministic and free of external side effects. Create all generated
clients and Soroban values inside the closure.

## Network WASM always wins

`contractimport!` uses the local WASM only to generate Rust types and method
bindings. Kanatoko never registers that file at the captured contract address.

Calls execute the contract instance and WASM captured from Stellar. The local
ABI artifact may be older; if it is incompatible with the captured network
contract, the call fails instead of silently running the local code.

Contracts without a local ABI use the same environment through dynamic calls:

```rust,ignore
let quote = fork.invoke::<u128>(
    &pool_id,
    "estimate_swap",
    (1_u32, 0_u32, 10_000_000_u128),
);
```

The caller supplies the return type; heterogeneous tuples use Soroban SDK value
conversions.

## Evidence boundary

Kanatoko proves contract-functional, state-reproducible Soroban Host behavior.
Local candidate installation and mocked authorization are explicit test
cheats. It does not claim transaction, fee, signature, Stellar Core/consensus,
SDEX, or arbitrary historical-ledger fidelity.

```sh
cargo test --locked
cargo test --locked --all-features
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
```

See the runnable mixed-mode acceptance in
[`tests/auto_runner.rs`](tests/auto_runner.rs) and the project principles in
[`MISSION.md`](MISSION.md).
