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
kanatoko = { version = "27", features = ["capture"] }
```

Choose the Kanatoko major that matches the ledger protocol being captured.
The selected Host executes finalized contract WASM built for that protocol or
an older one:

| Ledger protocol | Kanatoko | Contract WASM |
| --- | --- | --- |
| 25 | `kanatoko = "25"` | Protocol 25 or older |
| 26 | `kanatoko = "26"` | Protocol 26 or older |
| 27 | `kanatoko = "27"` | Protocol 27 or older |

## Your contract against mainnet

Suppose `my_vault.wasm` accepts a token address in its constructor and
`asset_decimals()` calls that token contract:

```rust,ignore
use kanatoko::mainnet;

mod app {
    use kanatoko::soroban_sdk;

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

## New code at a production address

Replace only the executable of an existing captured contract to test a new
version against its production storage:

```rust,ignore
use kanatoko::mainnet;

mod next {
    use kanatoko::soroban_sdk;

    soroban_sdk::contractimport!(
        file = "../target/wasm32v1-none/release/my_vault.wasm"
    );
}

const VAULT: &str = "C...";

#[test]
fn candidate_uses_production_state() {
    mainnet()
        .run(|fork| {
            let vault = fork.contract(VAULT);
            fork.replace_wasm(&vault, next::WASM);

            let upgraded = next::Client::new(fork.env(), &vault);
            assert!(upgraded.health_factor() > 0);
        })
        .unwrap();
}
```

`replace_wasm` preserves the address, instance/persistent/temporary storage,
and their TTLs. It does not call the constructor. Every captured contract that
calls the same address sees the replacement code.

This is a local code override, not an upgrade transaction: it does not call the
production upgrade method or test its authorization, delays, events, fees, or
signatures. If the replacement touches ledger keys absent from an older cache,
an online run recaptures them; offline mode fails closed until then.

The cache file is generated automatically under `.kanatoko/` from the selected
network, test thread name, and runner callsite. The first run discovers the
scenario's network dependencies; later runs replay the frozen ledger entirely
offline. Use `.cache(path)` to override the path, `.refresh()` to capture a
newer ledger, or `.offline()` in CI to require an existing cache.

## Network and RPC

On the Kanatoko line matching the public network's current protocol,
`mainnet()` and `testnet()` are symmetric and immediately usable with their
default RPC endpoints:

```rust,ignore
use kanatoko::{mainnet, testnet};

testnet()
    .run(|fork| {
        // Test against captured public testnet state.
    })
    .unwrap();

mainnet()
    .rpc_url("https://my-mainnet-rpc.example.com")
    .run(|fork| {
        // Same mainnet identity, another capture provider.
    })
    .unwrap();
```

An RPC override changes only the provider, never the selected network
passphrase. Capture fails if that provider reports another network. Automatic
cache paths include the network identity but not the RPC URL, so mainnet and
testnet cannot share a default cache while two providers for the same network
can. A cache hit does not parse or contact the configured RPC; `.refresh()`
forces capture and therefore validates the URL and network.

For lower-level capture, use `CaptureBuilder::mainnet(url)` or
`CaptureBuilder::testnet(url)`.

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
| `testnet()` | Selects Stellar public testnet with otherwise identical behavior. |
| `.rpc_url(url)` | Overrides the capture provider without changing network identity or automatic cache identity. |
| `.cache(path)` | Overrides the automatically derived cache path. |
| `.offline()` | Requires a cache hit and performs no discovery. |
| `.refresh()` | Captures a fresh coherent ledger. |
| `fork.contract("C...")` | Parses a network contract address. |
| `fork.account("G...")` | Parses a network account; Host access discovers its real account and trustlines. |
| `fork.muxed_account("M...")` | Parses a muxed address; ledger state belongs to its underlying G-address. |
| `fork.deploy(wasm, args)` | Locally installs candidate WASM and runs its constructor, if defined. |
| `fork.replace_wasm(contract, wasm)` | Replaces an existing WASM executable while preserving its address, storage, and TTL. |
| `fork.invoke(contract, fn, args)` | Invokes any contract without generated bindings. |
| `fork.try_invoke(contract, fn, args)` | Applies one call and returns a small typed failure instead of unwinding. |
| `fork.preview(contract, fn, args, auth)` | Simulates one call in an isolated child and returns detached result/auth/events/diff/resources. |
| `fork.local_account("label")` | Creates a deterministic local G-address with no ledger entry or funds. |
| `fork.fund_local_account(account, stroops)` | Explicitly creates or funds that local account through ledger injection. |
| `fork.mock_all_auths()` | Explicitly enables SDK record-and-mock authorization. |

Addresses belong in the scenario rather than `mainnet(...)` or `testnet(...)`.
Host execution discovers actual dependencies, including contracts reached only
through cross-contract calls.

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

Each Kanatoko major selects the same major of the SDK, Host, and
ledger-snapshot crates. Their broad major ranges let Cargo resolve one
compatible runtime with the rest of the test harness. Use the re-exported
`kanatoko::soroban_sdk`, `kanatoko::soroban_env_host`, and
`kanatoko::soroban_ledger_snapshot` instead of declaring a second runtime
version when possible.

A Kanatoko line can capture and replay a live network only when its selected
Host protocol matches the protocol reported by that network. For example, a
protocol-27 network requires Kanatoko 27; Kanatoko 25 or 26 is not a historical
network-state fork.

Your production contract does not need to upgrade with the harness. A contract
crate may remain on Soroban SDK 25 or 26, produce its normal network-valid
WASM, and let a Kanatoko 27 integration-test crate import that WASM when testing
against a protocol-27 network. The current Host executes the original older
WASM; its SDK generates only the test-side ABI client. Do not pass
environment-bound `Env`, `Address`, or `Val` values from the older contract SDK
into the harness.

Unsupported ledger-entry families and uncaptured keys fail closed.

```sh
cargo test --locked --all-features
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
```

See [MISSION.md](MISSION.md) for the project principles.
