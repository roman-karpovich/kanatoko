# Kanatoko

Test Soroban contracts against real Stellar state without deploying to a
public network.

Kanatoko runs your Rust scenario in one mutable Soroban environment. It
automatically discovers every network contract, WASM, account, trustline, and
storage entry the scenario touches, freezes them at one ledger, and replays the
test locally with no RPC fallback.

You can also install the contract you are developing into that fork. Your local
WASM and captured network contracts then call each other normally.

## Run the offline demo

Start with the complete [integrator demo](https://github.com/roman-karpovich/kanatoko-integrator-demo).
It runs a small DCA contract against the real Soroswap router and pool from a
committed mainnet capture.

With Git and Rust installed through rustup:

```sh
git clone https://github.com/roman-karpovich/kanatoko-integrator-demo.git
cd kanatoko-integrator-demo
cargo test --locked
```

The repository pins Rust 1.91.0 and includes the contract WASM and ledger
capture. The first build downloads the toolchain and Cargo dependencies as
needed; the tests themselves make no RPC calls. No Stellar CLI, wallet, faucet
or public-network deployment is needed.

The two tests check that:

- a locally installed contract swaps through the captured Soroswap contracts;
- an immediate retry fails with `TooEarly`, then advancing time by one day
  allows another swap;
- another account's swap moves the pool price below the DCA limit, producing
  `BelowLimit`;
- a read outside the captured state fails with `UnknownLedgerKeys`.

After the first build, `cargo test --locked --offline` reruns the tests without
downloading dependencies. The demo pins Kanatoko 29 and a Protocol 29 capture;
replaying it does not depend on the current mainnet protocol. Time changes,
funding and mocked authorization are explicit local test conditions.

For a concrete mock-versus-real discrepancy, see the independent
[SoroCron reproduction](https://github.com/roman-karpovich/kanatoko-sorocron-repro):
the example's mock accepts `deadline == now`, while the captured Soroswap router
rejects it. The linked repository pins the tested upstream revision; its tests
reproduce the failure rather than certify the example for production.

## Add Kanatoko to your tests

```toml
[dev-dependencies]
kanatoko = { version = "30", features = ["capture"] }
```

Choose the Kanatoko major that matches the ledger protocol being captured.
The selected Host executes finalized contract WASM built for that protocol or
an older one:

| Ledger protocol | Kanatoko | Contract WASM |
| --- | --- | --- |
| 27 | `kanatoko = "27"` | Protocol 27 or older |
| 28 | `kanatoko = "28"` | Protocol 28 or older |
| 29 | `kanatoko = "29"` | Protocol 29 or older |
| 30 | `kanatoko = "30"` | Protocol 30 or older |

The 30 line uses the stable Soroban Rust SDK and ledger snapshot 30 with the
compatible Host 30 runtime. Use the Kanatoko major matching the network
protocol being captured; the runner fails closed rather than replaying state
under a different Host. Mainnet and testnet both run Protocol 30, so capture
from them with this line. Kanatoko 29 cannot capture from either network any
more, but still replays committed Protocol 29 bundles offline; keep it pinned
for those, or refresh them on this line. Until a matching line exists, a
network exactly one protocol ahead can be captured and replayed through an
explicit opt-in; see [A network one protocol ahead](#a-network-one-protocol-ahead).

## Your contract against mainnet

Kanatoko 30 captures and replays Protocol 30 Mainnet state. The committed CLI
fixture is a Protocol 30 capture; its Protocol 29 predecessor and older
captures are retained separately only to test fail-closed cross-protocol
loading.

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
scenario's network dependencies. Each later online run probes the current
ledger: an exact anchor match replays the cache without ledger-entry reads,
while a changed anchor reloads every known key in coherent batches and then
discovers additions. State values from different ledgers are never mixed.
Use `.cache(path)` to override the path, `.refresh()` to force a cold capture
with an empty key inventory, or `.offline()` in CI to replay the cached ledger
as an explicitly pinned snapshot with zero transport calls.

## Offline example

`examples/offline_replay.rs` replays the committed Soroswap XLM/USDC Mainnet
capture and prints a 1 USDC to XLM quote, using only that bundle and no RPC:

```sh
cargo run --locked --features capture --example offline_replay
```

## Network and RPC

`mainnet()` and `testnet()` expose the same API and default RPC endpoints, but
each validates the selected network's exact live protocol. During a staggered
network upgrade, use the Kanatoko major matching each network independently:

```rust,ignore
use kanatoko::{mainnet, testnet};

testnet()
    .run(|fork| {
        // Test against captured public testnet state.
    })
    .unwrap();

mainnet()
    .rpc_url("https://my-mainnet-rpc.example.com")
    .rpc_rate_limit(5)
    .run(|fork| {
        // Same mainnet identity, another capture provider.
    })
    .unwrap();
```

An RPC override changes only the provider, never the selected network
passphrase. Capture fails if that provider reports another network. Automatic
cache paths include the network identity but not the RPC URL, so mainnet and
testnet cannot share a default cache while two providers for the same network
can. Every online run validates the configured RPC and probes its complete
latest-ledger anchor. An exact cache hit therefore performs `getNetwork` and
`getLatestLedger`, but no `getLedgerEntries`. Only `.offline()` ignores an
invalid RPC URL and makes no network calls. RPC pacing is disabled by default;
`.rpc_rate_limit(5)` opts into a five-request-per-second limit within each
capture run that also covers retry attempts. Separate capture runs do not
share a quota.

Cold capture retries throttled or transient read-only responses (`429`, `500`,
`502`, `503`, and `504`) up to four total attempts with `200/400/800 ms`
backoff. A reasonable numeric `Retry-After` is honored up to five seconds.
Other HTTP failures, transport failures, malformed responses, and JSON-RPC
errors fail immediately.

After retries are exhausted, Kanatoko stops issuing reads and returns a typed
`CaptureError::Transport`; a failed read is never mistaken for absent ledger
state. Capture discovery suppresses the inner Host panic output so the
transport error remains the actionable diagnostic.

Once ledger keys are known, Kanatoko fetches their coherent state in batches
of up to 200. This includes the complete inventory reused from an older ledger;
only the keys are reused, never their old Present/Absent values. The first
encounter with a newly discovered dynamic key remains a single read because
the returned value may determine which key the contract touches next.

Protocol 28 external executable references are closed transitively: Kanatoko
captures the owner's persistent executable-tag entry, validates its exact
32-byte WASM hash, and captures the referenced ContractCode. A missing or
malformed reference fails closed during capture and bundle loading.

For lower-level capture, use `CaptureBuilder::mainnet(url)` or
`CaptureBuilder::testnet(url)` and apply the same `.rpc_rate_limit(...)`
setting when needed.

Candidate code and its own storage remain local to each pass. Rebuilding it
does not invalidate the cache unless it starts touching new external ledger
keys. Online mode seeds recapture from the cached key inventory even when the
ledger anchor has not changed; offline mode fails closed until the cache is
refreshed.

## A network one protocol ahead

When a network upgrades before the matching Kanatoko line is available, a
Kanatoko line whose Host runs Protocol N can opt into capturing and replaying a
ledger exactly one protocol newer, Protocol N+1, and execute it on its own
Protocol N Host. On the 30 line that means a Protocol 31 ledger executed as
Protocol 30:

```rust,ignore
use kanatoko::mainnet;

mainnet()
    .allow_newer_protocol()
    .run(|fork| {
        // On the 30 line, against a Protocol 31 network.
        assert_eq!(fork.network_protocol_version(), 31);
        assert_eq!(fork.executed_protocol_version(), 30);
    })
    .unwrap();
```

This works only while Protocol N+1 keeps the ledger XDR that the Protocol N
line decodes. Protocol 30 changed the ledger header format, so the 29 line
cannot capture from Protocol 30 networks, also with the opt-in; use this line
for them. Since Mainnet and testnet both run Protocol 30, the opt-in has no
public network to apply to on this line until a network moves to Protocol 31.

The same opt-in exists on every API that checks the protocol:
`CaptureBuilder::allow_newer_protocol()`,
`CapturedFixture::from_file_allowing_newer_protocol(...)`,
`FrozenFixture::from_file_allowing_newer_protocol(...)` and
`FrozenFixture::from_snapshot_allowing_newer_protocol(...)`, and
`--allow-newer-protocol` on the CLI's `capture` and `run` commands. The flag
also applies to `.offline()` cache loads. Against a network on the Host's own
protocol the opt-in changes nothing.

- **One protocol only.** The mode accepts the Host's own protocol
  (`SUPPORTED_PROTOCOL_VERSION`) or exactly the next one. Older ledgers and
  ledgers two or more protocols ahead still fail with `UnsupportedProtocol`.
  Without the opt-in, behaviour and errors are unchanged.
- **Captures stay truthful.** Bundles, their ledger snapshot, provenance, and
  the ledger, inventory, and bundle digests keep the real network protocol. A
  Protocol N+1 bundle written this way has the same format and digests as one
  written by the Kanatoko N+1 line, and loaders without the opt-in reject it.
- **Only execution is downgraded.** Every `Env` gives the Host its own
  protocol as the ledger protocol, so a scenario's `Env` reports the executed
  protocol in its ledger info. Fork digests, receipt digests, and checkpoints
  keep the network protocol.
- **Traceable.** `network_protocol_version()` and
  `executed_protocol_version()` on `ScenarioFork`, `InvocationReport`,
  `StrictFork`, and `Fork`, plus
  `CaptureProvenance::executed_protocol_version()` and
  `FrozenFixture::executed_protocol_version()`, expose both protocols. The CLI
  prints a one-line warning to stderr and adds `networkProtocol` and
  `executedProtocol` to its report and to every receipt.

Behaviour that changed in the Host without an XDR change follows the older
Host. When Kanatoko 28.1.0 ran Protocol 29 on Host 28.0.2, that was WASM parse
metering (Protocol 29 also counts `br_table` targets and custom sections), the
removal of a separate WASM pre-validation pass, `extend_ttl` saturating instead
of failing on an overflowing TTL, and a Stellar Asset Contract error message.
A candidate installed locally recorded different `ContractCodeCostInputs` than
on a Protocol 29 Host, so ledger digests taken after that install differed,
while the committed Mainnet pool strict workflow returned identical quotes,
results, events, authorization trees, and state changes on both Hosts. Treat
resource estimates and metering-sensitive results as evidence from the older
Host.

Natural guards still fail closed and are never caught or softened: ledger data
the older XDR cannot decode, which capture reports as `UnsupportedProtocol` for
the network's protocol, and contracts whose WASM declares a newer interface
protocol (`contract protocol number is newer than host`). Move to the Kanatoko
major matching the network once it is available.

## One environment, both kinds of contract

`fork.deploy(...)` installs your candidate WASM and executes its constructor,
if defined. It returns an address for generated clients or dynamic calls:

```rust,ignore
let app_id = fork.deploy(app::WASM, constructor_args);
let typed = app::Client::new(fork.env(), &app_id);
let value = fork.invoke::<i128>(&app_id, "value", ());
```

Local IDs are deterministic, network-scoped hashes rather than the SDK's
small counter addresses, which can already hold balances on a live network.
An existing contract instance at the generated ID is still rejected.
Upgrading from counter-based IDs changes the keys touched by a scenario:
refresh its capture on a compatible network, or keep the original Kanatoko
version pinned for historical replay. Unknown keys remain errors.

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
| `.rpc_rate_limit(rps)` | Opts into per-capture read-RPC pacing; `0` keeps the default unlimited behavior. |
| `.cache(path)` | Overrides the automatically derived cache path. |
| `.offline()` | Replays the cached ledger as a pinned snapshot with zero RPC calls. |
| `.refresh()` | Discards cached key inventory and performs a cold coherent capture. |
| `.allow_newer_protocol()` | Accepts a network exactly one protocol ahead and executes it on the Host's own protocol; see above. |
| `.capture_network_config()` | Also captures the network's Soroban `ConfigSetting` entries for [signed transactions](#signed-transactions-on-a-strict-fork). |
| `.run_strict(\|fork\| …)` | Runs a transaction-faithful scenario on a discovering `StrictFork`; see [signed transactions](#signed-transactions-on-a-strict-fork). |
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
| `fork.capture_signer(&signer)` | Captures a test signer's account key, which must be absent for `StrictFork::create_local_account`. |
| `fork.capture_wasm(wasm)` | Captures the `ContractCode` key a transaction-faithful upload reads. |
| `fork.capture_deployed_contract(&deployer, salt)` | Captures the instance key at a `CreateContractV2` address and returns that address. |
| `fork.capture_ledger_key(&key)` | Captures any ledger key, present or absent, for later strict use. |

Addresses belong in the scenario rather than `mainnet(...)` or `testnet(...)`.
Host execution discovers actual dependencies, including contracts reached only
through cross-contract calls.

On an exact online cache hit or offline pinned replay, the closure runs once.
During a cold or inventory-seeded refresh it may run several times while
Kanatoko discovers the dependency graph and verifies strict replay. Every pass
starts from a fresh environment, so contract mutations do not accumulate
between passes. Only effects outside the Soroban environment, such as random
generation, file writes, HTTP requests, counters, or output, would be repeated.

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

### Controlling time

A scenario can move ledger time with the SDK's own `Ledger` test utility.
Kanatoko re-exports `soroban_sdk`, so no separate dependency is needed:

```rust,ignore
use kanatoko::soroban_sdk::testutils::Ledger as _;

let now = fork.env().ledger().timestamp();
fork.env().ledger().set_timestamp(now + 86_400);
```

Limits to keep in mind:

- Timestamp and sequence number are separate. `set_timestamp` does not move the
  sequence number; `set_sequence_number` is the call for that.
- Temporary entries can expire as the sequence number advances.
- Persistent entries past their TTL stay readable because state archival is not
  modeled, so a passing test does not prove behavior around state archival.

## Signed transactions on a strict fork

`run_strict` runs a scenario on a `StrictFork`, which fails closed on every
ledger key the fixture lacks, and executes host functions as Stellar
transactions: simulated by the preflight library of RPC
`simulateTransaction`, signed, then validated, charged, and applied as
Stellar Core does in the next ledger. The scenario is written once: online,
a discovery pass reads every key the fixture lacks from the network at the
round's ledger and runs on, so it reaches the keys behind it (a contract's
code, its storage, the contracts it calls). The recorded keys are then
rematerialized at one coherent ledger and the pass reruns until it records
none, usually after two rounds. The frozen fixture then replays strictly
offline.

```rust,ignore
use kanatoko::{mainnet, Ed25519Signer, TransactionCall};
use kanatoko::soroban_env_host::xdr::ScAddress;

const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";

#[test]
fn signed_deploy_and_call() {
    let owner = Ed25519Signer::from_label("owner");
    let run = mainnet().run_strict(|fork| -> Result<(), Box<dyn std::error::Error>> {
        fork.create_local_account(&owner, 1_000 * 10_000_000)?;
        let app = fork.deploy(&owner, MY_WASM, [1; 32], (41_i64,))?.address;

        let call = TransactionCall::invoke_fn(&owner, &app, "authorized_increment", (&owner, 5_i64))?;
        let receipt = fork.apply_signed(&call, &[&owner])?;
        println!("{receipt}");
        assert_eq!(fork.view::<i64>(&app, "get", ())?, 46);

        let xlm: ScAddress = XLM.parse()?;
        fork.set_token_balance(&xlm, &app, 25)?;
        assert_eq!(fork.token_balance(&xlm, &app)?, 25);
        Ok(())
    });
    run.unwrap().into_value().unwrap();
}
```

`create_local_account`, `deploy`, and every other call discover their own
keys: the signer's account must be absent on the network, and the uploaded
code and the deployment address are captured like any other key. Negative
paths run inside the scenario, so the keys only they read are captured too.
Discovery passes only collect keys; the final pass on the frozen fixture
is a plain strict run. A key missing from a transaction's declared
footprint never reaches a lookup: it fails in the Host as in Core and is
never fetched. The scenario must be
deterministic; each test freezes its own cache, or several tests replay
parts of one flow offline against a fixture that one refresh test captured
(see `tests/strict_run.rs`).

The typed helpers sit on the explicit transaction API below them:

| API | Meaning |
| --- | --- |
| `TransactionCall::invoke_fn(&source, &contract, "fn", args)` | A contract call with tuple arguments of `IntoScVal` values. |
| `fork.sign(&call, &signers)` / `fork.apply_signed(&call, &signers)` | Simulate, authorize with deterministic hashed nonces and a 100-ledger expiry, prepare, and apply; only a commit is `Ok`. |
| `fork.deploy(&deployer, wasm, salt, args)` | Upload and `CreateContractV2` with the constructor, both signed by the deployer. |
| `fork.view::<R>(&contract, "fn", args)` | A read-only call against the last closed ledger, converted with `TryFromVal`. |
| `fork.token_balance(&token, &holder)` / `fork.account_balance(&account)` | Token and native balances. |
| `fork.set_token_balance(&token, &holder, amount)` | Explicit override of a Stellar Asset Contract balance (contract `Balance`, account, or trustline) to a state the network can reach, with the issuer's flags. |
| `fork.contract_data::<V>(…)` / `fork.update_contract_data(…, \|env, v: &mut V\| …)` | Typed contract data; `Map<Symbol, Val>` works for foreign types. |
| `fork.contract_instance(&contract)` | Executable and instance storage. |
| `fork.create_local_accounts(&signers, stroops)` / `fork.scoped(\|fork\| …)` | Several accounts; a block reverted afterwards. |
| `prepared.resigned_by(&fork, i, &signer)`, `.with_function(f)`, `.with_args(args)` | Negative variants of a prepared transaction. |
| `receipt.events_from(&contract, "topic")`, `receipt.single_event(&contract, "topic")` | Matching events; `event.topic_as::<T>(i)` and `event.data_as::<T>()` decode them. |
| `fork.expect_rejected_with(&prepared, &error)`, `failure.host_error()` | An included failure with exactly this Host error and only the fee and sequence charged. |
| `receipt.resources.check_within(&limits.scaled(80))` | Asserts measured resources against the limits, here with 20% headroom; receipts print a one-line summary. |
| `sdk(\|env\| value)`, `to_sc_val(\|env\| value)`, `from_sc_val::<T>(&val)`, `from_sc_val_in::<T>(env, &val)` | `#[contracttype]` structs, enums, vectors, and maps without an `Env` of your own; only XDR crosses into the fork. |

An SDK object such as an `Address` or `Vec` is only a handle into the `Env`
that made it, and the SDK converts it by that handle alone, so a value from
another `Env` would silently stand for a different object. Build SDK values
inside the closure, in the `Env` it is given; the closure runs more than once,
and a value holding an object of any other environment, including one
returned by `from_sc_val`, is refused with `ConversionError`:

```rust,ignore
use kanatoko::soroban_sdk::Address;
use kanatoko::{sdk, to_sc_val};

let key = sdk(|env| DataKey::Position(Address::from_str(env, OWNER)));
let value = to_sc_val(|env| Position { owner: Address::from_str(env, OWNER), amount: 7 })?;
fork.set_contract_data(&app, key, durability, Some(value))?;
```

The explicit flow is available underneath. From a fixture captured with
`.capture_network_config()`:

```rust,ignore
use kanatoko::{mainnet, Ed25519Signer, InvokeRequest, ReceiptDisposition, TransactionCall};
use kanatoko::soroban_env_host::xdr::{Int128Parts, ScAddress, ScSymbol, ScVal};

const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";

#[test]
fn signed_transfer() -> Result<(), Box<dyn std::error::Error>> {
    let sender = Ed25519Signer::from_label("sender");
    let relayer = Ed25519Signer::from_label("relayer");
    let run = mainnet().capture_network_config().run(|fork| {
        let xlm = fork.contract(XLM);
        fork.invoke::<u32>(&xlm, "decimals", ());
        fork.capture_signer(&sender);
        fork.capture_signer(&relayer);
    })?;
    let mut fork = run.fixture().fork();

    // Test accounts are explicit local state at captured-absent keys.
    fork.create_local_account(&sender, 100_000_000)?;
    fork.create_local_account(&relayer, 100_000_000)?;

    let transfer = InvokeRequest {
        contract: XLM.parse::<ScAddress>()?,
        function: ScSymbol("transfer".try_into()?),
        args: vec![
            ScVal::Address(sender.address()),
            ScVal::Address(relayer.address()),
            ScVal::I128(Int128Parts { hi: 0, lo: 25 }),
        ],
    };
    // The relayer is the transaction source; the sender signs its own tree.
    let call = TransactionCall::invoke(&relayer, &transfer)?;
    let simulation = fork.simulate_transaction(&call)?;
    let expiration = fork.ledger_sequence() + 100;
    let authorization = fork.authorize(&simulation, &[&sender], 1, expiration)?;
    let transaction = fork.prepare_transaction(&call, authorization)?;

    let receipt = fork.apply_transaction(&transaction)?;
    assert_eq!(receipt.disposition, ReceiptDisposition::Committed);

    // Replaying it reuses the nonce: included, charged, and failed.
    let replay = fork.expect_failed_apply(&transaction)?;
    assert!(matches!(replay, kanatoko::FailedApply::Rejected(_)));
    Ok(())
}
```

`authorize` gives the transaction source source-account credentials and
signs every other tree with the matching signer, in the
`SorobanCredentials::Address` format the Stellar SDKs use;
`authorize_as(AuthorizationFormat::AddressV2, ...)` signs `AddressV2`, which
Core accepts from Protocol 27. `sign_authorization` signs one tree, including
one from `Receipt::authorization` of an `invoke`, and `invoke_with_source`
names the transaction source for `invoke` under `AuthMode::Enforce`.
`sign_transaction` combines simulate, authorize, and prepare.

Upload and `CreateContractV2` run the same way, under the deployer's real
authorization, and prove the production contract address and constructor.
Capture the code key and the deployment address in the scenario with
`fork.capture_wasm(MY_WASM)` and
`fork.capture_deployed_contract(&deployer.address(), salt)`:

```rust,ignore
fork.create_local_account(&deployer, 10_000_000_000)?;
let expiration = fork.ledger_sequence() + 100;

let upload = TransactionCall::upload_wasm(&deployer, MY_WASM)?;
let transaction = fork.sign_transaction(&upload, &[], 0, expiration)?;
fork.apply_transaction(&transaction)?;

let create = TransactionCall::create_contract(
    &deployer,
    deployer.address(),
    salt,
    MY_WASM_SHA256,
    vec![ScVal::I64(41)],
)?;
let transaction = fork.sign_transaction(&create, &[], 0, expiration)?;
let receipt = fork.apply_transaction(&transaction)?;
let expected = fork.contract_address(&deployer.address(), salt)?;
assert_eq!(receipt.outcome, InvokeOutcome::Success(ScVal::Address(expected)));
```

What happens where:

- Simulation and preparation run against the fork's last closed ledger `N`,
  as RPC simulates against its latest ledger, with RPC's default
  adjustments: instructions `max(1.04x, +50,000)`, exact read and write
  bytes, a fee for the largest envelope plus 10% (at least 500 bytes), and
  the refundable fee plus 15%. An entry or signature that lives until `N`
  is therefore live in simulation but expired when the transaction applies
  in `N + 1`, as on the network.
- `apply_transaction` refuses, with `TransactionError::Invalid` and no change,
  what Core would not include: declared resources, footprint counts, key and
  envelope sizes above the captured limits; invalid footprint keys, duplicates,
  or archived-entry indices; a memo or muxed source; a resource fee below the
  non-refundable fee or an inclusion fee below the base fee; time bounds; a
  missing source, a source key below its medium threshold, a balance below the
  full fee, and frozen ledger keys.
- An included transaction closes ledger `N + 1`, at the previous close time
  plus the network's target close time. The source pays the resource fee plus
  the inclusion fee, capped at the 100-stroop base fee, and its sequence
  number advances. The Host runs with the enforcing footprint; a key outside
  it fails as on the network. Of the entries marked for restore, only those
  still archived are restored. Archived entries not marked for restore, too
  many read or written bytes, entries above the size limits (restored ones
  included), events above the limit, or a refundable fee
  below events plus rent fail the transaction with Core's result code. The
  unused refundable fee is refunded; a failed transaction keeps only its
  non-refundable and inclusion fees charged and returns
  `TransactionError::Rejected`.
- `expect_failed_apply` requires a network failure and proves it atomic:
  nothing changed for a refused transaction, only the source's fee and
  sequence number for a failed one. Fixture and configuration errors stay
  errors. Negative tests derive variants with `with_tampered_signature`,
  `with_authorization`, `without_authorization_entry`,
  `without_footprint_key`, `with_transaction_data`, and `with_call`.

Without captured network settings every transaction method fails with
`TransactionError::MissingNetworkConfig`. `create_local_account` and
`set_contract_data` are explicit local overrides for test setup, not
transactions; `create_local_account` refuses an account key the capture did
not confirm absent, so a test never shadows a real account.

## Roadmap

Kanatoko is moving toward being the easiest way to test your own Soroban contract on top of real deployed protocols, under conditions that are hard to reproduce on a live network.

Planned improvements, in priority order:

- **Scenario controls:** documented one-line control of time, balances, storage, and external inputs such as oracle prices.
- **Clear failure explanations:** which external call failed and why, with a readable cross-contract call tree.
- **Network fidelity:** cost-model compatibility checks and transaction-faithful calls from the scenario facade.
- **Integrator onboarding:** a project template and recipes for building on AMMs, lending pools, and oracles.

These are plans, not shipped features.

## Evidence boundary

`fork.deploy(...)`, local account funding, and mocked authorization are explicit
test mechanics. They prove contract behavior in the Soroban Host; they do not
emulate transaction envelopes, deployment authorization, signatures, fees,
Stellar Core consensus, or SDEX execution.

`StrictFork::apply_transaction` verifies authorization signatures, the
footprint, Core's validity rules, and the captured network's per-transaction
limits, and charges fees computed with the Host's fee functions; recomputed
fees of real Mainnet transactions equal what the network charged. It does not
verify envelope signatures, model transaction sets, surge pricing, or other
transactions in the same ledger, and assumes the 100-stroop base fee. Ledger
close times advance by the network's target close time.

Preview resources are the raw local Host estimate, not fee parity. Kanatoko
keeps typed `ScError` and raw XDR evidence and does not derive stable behavior
by parsing diagnostic or panic text.

Each stable Kanatoko major selects the same major of the SDK, Host, and
ledger-snapshot crates. Stable lines use broad major ranges so Cargo can
resolve one compatible runtime with the rest of the test harness. Prerelease
lines use exact compatible pins. Use the re-exported
`kanatoko::soroban_sdk`, `kanatoko::soroban_env_host`, and
`kanatoko::soroban_ledger_snapshot` instead of declaring a second runtime
version when possible.

A Kanatoko line can capture and replay a live network only when its selected
Host protocol matches the protocol reported by that network. For example, a
protocol-27 network requires Kanatoko 27; no other major is a historical
network-state fork of it. The only exception is the explicit newer-protocol
opt-in, limited to one protocol ahead and executed on the older Host.

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
