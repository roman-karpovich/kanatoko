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

#[test]
fn swap_moves_the_real_pool_price() {
    mainnet(POOL)
        .cache(".kanatoko/aquarius-swap.json")
        .run(|fork| {
            let env = fork.env();
            let pool_id = fork.contract(POOL);
            let usdc = fork.contract(USDC);
            let user = fork.local_account("swap-user");

            // Typed calls and dynamic calls share this Env and its mutations.
            let pool = pool_abi::Client::new(env, &pool_id);
            let before = pool.estimate_swap(&1, &0, &10_000_000);
            let reserves = pool.get_reserves();
            let amount = reserves.get(1).unwrap() / 10;

            fork.mock_all_auths();
            let admin = fork.invoke::<Address>(&usdc, "admin", ());

            // This is a real G-address, so initialize its classic USDC state.
            fork.invoke::<()>(&usdc, "trust", (user.clone(),));
            fork.invoke::<()>(&usdc, "set_authorized", (user.clone(), true));
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

`local_account("swap-user")` creates a funded G-account only in the local
ledger. Its address is pseudorandom but stable for the network, root contract,
and label, so capture and replay touch identical keys. It has no private key:
authorization remains an explicit test mode. Classic assets still require
their real SAC `trust` flow, and `set_authorized` when applicable.

## Real accounts and M-addresses

`fork.account("G...")` references an existing Stellar account without
modifying it. State is fetched lazily when the scenario actually uses it:

```rust,ignore
let holder = fork.account("G...");
let xlm_balance = fork.invoke::<i128>(&xlm, "balance", (holder.clone(),));
let usdc_balance = fork.invoke::<i128>(&usdc, "balance", (holder,));
```

For a G-address, XLM SAC `balance` reads the complete network `AccountEntry`;
a classic asset SAC reads its `TrustLineEntry`. Kanatoko captures those exact
entries at the same ledger as the contracts and replays them offline. The same
automatic rule covers every touched Host-supported Account, Trustline,
ContractData, and ContractCode key. Unsupported classic-ledger families fail
closed instead of being silently omitted. XLM `balance` is the account's total
ledger balance, not its spendable balance after reserves and liabilities.

`fork.muxed_account("M...")` parses an actual multiplexed account. Protocol 27
SAC supports `MuxedAddress` as a `transfer` destination: balance changes apply
to the underlying G-account while the multiplexing ID is emitted as event
metadata. `balance`, `trust`, `mint`, authorization, and transfer sources still
use the underlying `Address`, not an M-address.

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
