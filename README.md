# Kanatoko

Kanatoko is a Rust-first, Stellar-native local fork toolkit for Soroban. It
turns a coherent captured ledger into a strict mutable Host session where
candidate production WASM can call captured contracts, several calls share
state, and every call produces detached XDR evidence.

The current M3 workflow is intentionally narrower than a full Anvil daemon. It
is useful today for the committed Aquarius XLM/USDC constant-product fixture:

- Present, RPC-confirmed Absent, and Unknown ledger keys remain distinct after
  mutation and checkpoint/revert;
- candidate WASM is SHA-256 checked and installed through the SDK's test-only
  local `register_at` path with its constructor;
- generic preview/apply calls return result/error, authorization tree and mode,
  contract events, diagnostics, exact state diff, and before/after digest;
- authorization is Stellar-native: record, atomic mock-exact
  record-and-compare, or Host enforcement;
- all invocation inputs and receipt outputs are detached `Sc*`/XDR values, so
  replacing the owned `Env` never leaves a client or `Val` in the API;
- strict replay owns no RPC transport and reports zero upstream reads.

## Run the M3 acceptance

The command below runs only against the committed capture. Dead proxy settings
are optional; they are shown to make an accidental network fallback obvious.

```sh
HTTP_PROXY=http://127.0.0.1:9 \
HTTPS_PROXY=http://127.0.0.1:9 \
ALL_PROXY=http://127.0.0.1:9 \
NO_PROXY= \
cargo run --locked --offline --features capture --bin kanatoko -- \
  run aquarius-cp --format text
```

For machine-inspectable detached receipts:

```sh
cargo run --locked --offline --features capture --bin kanatoko -- \
  run aquarius-cp --format json
```

The run performs:

1. strict-load mainnet capture ledger `63600296`;
2. locally inject the hash-pinned Aquarius wrapper candidate and run its
   constructor with the captured pool address;
3. estimate 1 USDC to XLM through candidate -> captured pool;
4. mint the synthetic user 10% of the captured USDC reserve in record mode;
5. preview the candidate swap, capture its exact
   wrapper -> pool -> USDC authorization tree, then apply only if the tree
   matches exactly;
6. prove the quote and reserves changed, revert, and prove the first quote was
   restored;
7. reject an uncaptured key and report zero upstream reads.

The bundled candidate source is in
`fixtures/contracts/aquarius-wrapper/`; its committed optimized WASM SHA-256 is
`798c959e1e22093c49b4ec6636aafed14e889614fb243426abe5023b30c17520`.
A custom ABI-compatible wrapper can be supplied with both
`--candidate-wasm PATH` and `--candidate-sha256 HEX`.

## Capture by address

This is the network-backed exploratory step; it performs read-only RPC calls
and writes a bundle, then reloads that bundle for a strict offline replay.

```sh
cargo run --locked --features capture --bin kanatoko -- \
  capture aquarius-cp \
  --rpc-url https://mainnet.sorobanrpc.com \
  --root CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE \
  --bundle /tmp/aquarius-capture.json
```

The root address is not a claim to cover unexecuted branches. Capture freezes
every Host-touched key for the exercised scenario at one coherent latest-ledger
boundary, including confirmed absence.

## Rust API shape

```rust,ignore
let captured = CapturedFixture::from_file("capture.json", MAINNET_PASSPHRASE)?;
let mut fork = captured.fork();
let checkpoint = fork.checkpoint();

let preview = fork.invoke(request.clone(), ExecutionMode::Preview, AuthMode::Record)?;
let applied = fork.invoke(
    request,
    ExecutionMode::Apply,
    AuthMode::MockExact(preview.authorization.clone()),
)?;

fork.revert(checkpoint)?;
```

`MockExact` executes in an isolated recording child and promotes it only when
the observed tree equals the supplied detached tree. Recording satisfies the
auth it discovers, so it is mock evidence, not signature evidence.
Recording also creates temporary anti-replay nonce entries as Host simulation
scaffolding. M3 permits those otherwise-uncaptured keys only in `Record` and
`MockExact`, and removes newly generated mock nonces before calculating the
receipt diff/digest or promoting state. `Enforce` gets no such waiver: a real
nonce read must be captured as Present/confirmed Absent or belong to an
explicitly local candidate contract.

## Verification

```sh
cargo test --locked --all-features
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Default-feature M0/M1 tests remain supported; M2/M3 APIs and the CLI require
the `capture` feature.

## Evidence boundary

M3 demonstrates contract-functional and state-reproducible local Host
execution. Local candidate installation is an explicit test cheat. Kanatoko
does not claim transaction-faithful upload/create, envelope signatures,
source-account sequence, fees, timebounds, admission, Stellar Core/consensus,
SDEX, arbitrary historical ledgers, or a JSON-RPC daemon. SDK v27 also cannot
combine a custom strict snapshot source with restored SDK test generators, so
strict checkpoint/revert restores ledger, coverage, and local candidate state
but does not claim generator or Host PRNG continuity.

The Aquarius acceptance exercises `Record` and `MockExact`; the `Enforce` path
is wired directly to Host authorization entries and covers strict nonce policy
plus malformed-input atomicity, but a successful non-empty signed `Enforce`
fixture is not yet claimed.

See [MISSION.md](MISSION.md) for product principles and
`fixtures/mainnet/aquarius-xlm-usdc-cp/README.md` for fixture provenance.
