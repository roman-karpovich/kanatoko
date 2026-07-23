# Kanatoko

Test Soroban contracts against real Stellar state without deploying to a
public network.

Kanatoko runs your Rust scenario in one mutable Soroban environment. It
automatically discovers every network contract, WASM, account, trustline, and
storage entry the scenario touches, freezes them at one ledger, and replays the
test locally with no RPC fallback.

You can also install the contract you are developing into that fork. Your local
WASM and captured network contracts then call each other normally.

```toml
[dev-dependencies]
kanatoko = { git = "https://github.com/roman-karpovich/kanatoko", features = ["capture"] }
soroban-sdk = { version = "=27.0.0", features = ["testutils"] }
```

## Your contract against mainnet

Suppose `my_vault.wasm` accepts a token address in its constructor and
`asset_decimals()` calls that token contract:

```rust,ignore
use kanatoko::mainnet;

mod app {
    soroban_sdk::contractimport!(
        // Adjust this path to your build artifact.
        file = "../target/wasm32v1-none/release/my_vault.wasm"
    );
}

// Native XLM Stellar Asset Contract on mainnet.
const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";

#[test]
fn local_contract_uses_mainnet_state() {
    mainnet()
        .run(|fork| {
            let xlm = fork.contract(XLM);

            // This WASM is installed locally and its constructor runs.
            let app_id = fork.deploy(app::WASM, (xlm,));
            let app = app::Client::new(fork.env(), &app_id);

            // Local WASM -> captured mainnet XLM contract.
            assert_eq!(app.asset_decimals(), 7);
        })
        .unwrap();
}
```

The cache file is generated automatically under `.kanatoko/` from the test
thread name and the `mainnet()` callsite. The first run discovers the
scenario's network dependencies; later runs replay the frozen ledger entirely
offline. Use `.cache(path)` to override the path, `.refresh()` to capture a
newer ledger, or `.offline()` in CI to require an existing cache.

Candidate code and its own storage remain local to each pass. Rebuilding it
does not invalidate the cache unless it starts touching new external ledger
keys. Online mode then recaptures the complete scenario atomically; offline
mode fails closed until the cache is refreshed.

## One environment, both kinds of contract

`fork.deploy(...)` installs your candidate WASM and executes its constructor,
if defined. It returns an address for generated clients or dynamic calls:

```rust,ignore
let app_id = fork.deploy(app::WASM, constructor_args);
let typed = app::Client::new(fork.env(), &app_id);
let value = fork.invoke::<i128>(&app_id, "value", ());
```

For a captured address, a local `contractimport!` file supplies only Rust
bindings. Calls still execute the instance and WASM loaded from Stellar. A
stale incompatible ABI fails instead of silently replacing network code.

Contracts without local bindings can be invoked dynamically:

```rust,ignore
let dependency = fork.contract(DEPENDENCY);
let value = fork.invoke::<u32>(&dependency, "decimals", ());
```

Use `try_invoke` when a failure is part of the test rather than a panic:

```rust,ignore
let result = fork.try_invoke::<u32>(&dependency, "decimals", ());
assert!(result.is_ok());
```

Preview executes a mutating call in an isolated child and returns detached
evidence without changing the scenario state:

```rust,ignore
use kanatoko::PreviewAuth;

let report = fork
    .preview(&vault, "deposit", (user, amount), PreviewAuth::Record)
    .unwrap();
let shares = report.result::<i128>(fork.env()).unwrap();
assert!(!report.state_changes().is_empty());
```

`PreviewAuth::Record` exposes the requested authorization tree.
`PreviewAuth::Exact(recorded.authorization().to_vec())` reruns the preview and
requires that exact tree. Exact validation is preview-only.

Typed clients, dynamic calls, local WASM, and captured contracts share the same
mutable state and can be mixed freely.

## Scenario API

| API | Meaning |
| --- | --- |
| `mainnet()` | Selects Stellar mainnet and derives a scenario cache path; it does not privilege one root contract. |
| `.cache(path)` | Overrides the automatically derived cache path. |
| `.offline()` | Requires a cache hit and performs no discovery. |
| `.refresh()` | Captures a fresh coherent ledger. |
| `fork.contract("C...")` | Parses a network contract address. |
| `fork.account("G...")` | Parses a network account; Host access discovers its real account and trustlines. |
| `fork.muxed_account("M...")` | Parses a muxed address; ledger state belongs to its underlying G-address. |
| `fork.deploy(wasm, args)` | Locally installs candidate WASM and runs its constructor, if defined. |
| `fork.invoke(contract, fn, args)` | Invokes any contract without generated bindings. |
| `fork.try_invoke(contract, fn, args)` | Applies one call and returns a small typed failure instead of unwinding. |
| `fork.preview(contract, fn, args, auth)` | Simulates one call in an isolated child and returns detached result/auth/events/diff/resources. |
| `fork.local_account("label")` | Creates a deterministic local G-address with no ledger entry or funds. |
| `fork.fund_local_account(account, stroops)` | Explicitly creates or funds that local account through ledger injection. |
| `fork.mock_all_auths()` | Explicitly enables SDK record-and-mock authorization. |

Addresses belong in the scenario rather than `mainnet(...)`. Host execution
discovers actual dependencies, including contracts reached only through
cross-contract calls.

On a cache hit, the closure runs once. During a cold capture it may run several
times while Kanatoko discovers the dependency graph and verifies strict
replay. Every pass starts from a fresh environment, so contract mutations do
not accumulate between passes. Only effects outside the Soroban environment,
such as random generation, file writes, HTTP requests, counters, or output,
would be repeated.

Generate one-time inputs before `.run(...)` and capture ordinary Rust values in
the closure. Create environment-bound values, addresses, and generated clients
inside it:

```rust,ignore
let amount = generate_amount_once();

mainnet()
    .run(|fork| {
        let pool = fork.contract(POOL);
        fork.invoke::<()>(&pool, "deposit", (amount,));
    })
    .unwrap();
```

A test may call `.run(...)` more than once. Each call creates an independent
fork, and ordinary Rust code between the calls runs once. Use a separate cache
path for a materially different scenario. If calls must observe each other's
contract mutations, keep them inside the same `.run(...)` closure.

## Evidence boundary

`fork.deploy(...)`, local account funding, and mocked authorization are explicit
test mechanics. They prove contract behavior in the Soroban Host; they do not
emulate transaction envelopes, deployment authorization, signatures, fees,
Stellar Core consensus, or SDEX execution.

Preview resources are the raw local Host estimate, not fee parity. Kanatoko
keeps typed `ScError` and raw XDR evidence and does not derive stable behavior
by parsing diagnostic or panic text.

The current release is pinned to protocol 27. Unsupported ledger-entry families
and uncaptured keys fail closed.

```sh
cargo test --locked --all-features
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
```

See [MISSION.md](MISSION.md) for the project principles.
