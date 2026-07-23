# Kanatoko

Run Soroban Rust tests against a coherent snapshot of real Stellar state —
locally, deterministically, and without deploying or spending funds.

Point Kanatoko at one deployed contract and write the scenario once. On the
first run it discovers the contracts, network WASM, accounts, trustlines, and
contract storage that the scenario actually touches. It then freezes one
ledger and runs the same scenario against that sealed state with zero RPC
fallback.

No capture script. No hand-written fixture. No local replacement for the
contracts under test.

```toml
[dev-dependencies]
kanatoko = { git = "https://github.com/roman-karpovich/kanatoko", features = ["capture"] }
soroban-sdk = { version = "=27.0.0", features = ["testutils"] }
```

## Example: move a real Aquarius pool price

This test loads the mainnet XLM/USDC pool and USDC token contract, creates a
local Stellar account, mints 10% of the pool's USDC reserve to it, swaps the
USDC, and proves that the pool price changed.

```rust,ignore
use kanatoko::mainnet;

mod pool_abi {
    // Any ABI-compatible build is enough. This file is never executed.
    soroban_sdk::contractimport!(file = "tests/wasm/aquarius_pool_abi.wasm");
}

const POOL: &str = "CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE";
const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

#[test]
fn swap_moves_the_real_pool_price() {
    mainnet(POOL)
        .cache(".kanatoko/aquarius-swap.json")
        .run(|fork| {
            let pool_id = fork.root().clone();
            let usdc = fork.contract(USDC);
            let user = fork.local_account("swap-user");

            // Generated clients and dynamic calls share one mutable Env.
            let pool = pool_abi::Client::new(fork.env(), &pool_id);
            let before = pool.estimate_swap(&1, &0, &10_000_000);
            let reserves = pool.get_reserves();
            let amount = reserves.get(1).unwrap() / 10;

            fork.mock_all_auths();
            fork.invoke::<()>(&usdc, "trust", (user.clone(),));
            fork.invoke::<()>(&usdc, "set_authorized", (user.clone(), true));
            fork.invoke::<()>(
                &usdc,
                "mint",
                (user.clone(), i128::try_from(amount).unwrap()),
            );

            let received = fork.invoke::<u128>(
                &pool_id,
                "swap",
                (user, 1_u32, 0_u32, amount, 0_u128),
            );
            assert!(received > 0);

            let after = pool.estimate_swap(&1, &0, &10_000_000);
            assert!(after < before);
        })
        .unwrap();
}
```

The cache is created automatically after a successful strict replay; it is not
something you prepare or edit by hand. A cache hit is fully offline. Use
`.refresh()` to capture a newer mainnet ledger, or `.offline()` to require an
existing cache and forbid discovery in CI. If an online run reaches a key that
the cache does not cover, Kanatoko recaptures the complete scenario at one
coherent ledger and replaces the cache atomically.

The scenario can run several times while Kanatoko finds the dependency fixed
point, so keep it deterministic and free of external side effects. Create
generated clients and Soroban values inside the closure.

## What `mainnet(POOL)` means

`POOL` is the initial contract and cache identity, not a whitelist. Its
instance and, when applicable, referenced WASM are loaded first. The USDC
Stellar Asset Contract and every other Host key reached by the executed
scenario are then captured from the same ledger. WASM-backed contracts execute
their captured network WASM; Stellar Asset Contracts execute natively.

Discovery is execution-driven: Kanatoko captures the keys used by this
scenario path, including confirmed-absent keys, rather than every key a
contract could possibly access.

## Typed and dynamic calls

A local WASM passed to `contractimport!` is an ABI source only. Kanatoko never
registers it at the captured address. Calls always execute the contract
instance and WASM captured from Stellar; a stale incompatible ABI fails instead
of silently replacing upgraded network code.

Contracts without a local ABI use dynamic invocation:

```rust,ignore
let quote = fork.invoke::<u128>(
    &pool_id,
    "estimate_swap",
    (1_u32, 0_u32, 10_000_000_u128),
);
```

Both styles use the same `Env`, so they can be mixed freely in one stateful
scenario.

## Stellar addresses

| API | Meaning |
| --- | --- |
| `fork.contract("C...")` | Parses a contract address; later Host access discovers its instance, WASM, and storage. |
| `fork.account("G...")` | Parses an account address; later Host access discovers its network account and trustlines. |
| `fork.muxed_account("M...")` | Parses muxed metadata; ledger state belongs to the underlying G-address. |
| `fork.local_account("label")` | Injects a funded, deterministic local G-account with no private key or initial trustlines. |

For a real G-address, XLM SAC `balance` reads its complete `AccountEntry`, and
a classic asset SAC reads its `TrustLineEntry`. Kanatoko captures those exact
entries at the same ledger as the contracts. A local account starts with only
an injected `AccountEntry`, so classic assets still require their real `trust`
flow and, when applicable, `set_authorized`.

On protocol 27, `MuxedAddress` is supported as a SAC `transfer` destination.
The balance change applies to the underlying G-account and the multiplexing ID
is emitted as event metadata. Balance queries, trust, mint, authorization, and
transfer sources still use the underlying `Address`.

## More control

The lower-level `CapturedFixture` and `StrictFork` APIs add local candidate
WASM registration, explicit authorization modes, receipts, state diffs, and
checkpoint/revert. These operations are labelled local test mechanics; they
are not disguised as network transactions.

## Evidence boundary

Kanatoko proves contract-functional, state-reproducible Soroban Host behavior.
It does not prove transaction envelopes, signatures, fees, Stellar Core
consensus, SDEX behavior, or arbitrary historical-ledger fidelity.

The current release is pinned to protocol 27. Host-supported `Account`,
`Trustline`, `ContractData`, and `ContractCode` entries are captured
automatically; unsupported classic-ledger families fail closed. XLM SAC
`balance` is the total ledger balance, not spendable balance after reserves and
liabilities. Local accounts and mocked authorization are explicit test cheats.

```sh
cargo test --locked --features capture --test auto_runner
cargo test --locked --all-features
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
```

See the runnable acceptance scenario in
[`tests/auto_runner.rs`](tests/auto_runner.rs), strict mutation examples in
[`tests/strict_fork.rs`](tests/strict_fork.rs), and the project principles in
[`MISSION.md`](MISSION.md).
