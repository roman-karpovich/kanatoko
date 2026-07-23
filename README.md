# Kanatoko

**Run Soroban contracts against captured Stellar mainnet state — locally,
statefully, and offline.**

Kanatoko turns a contract address and an exercised scenario into a strict,
mutable Soroban Host fork. It captures production WASM and ledger state,
installs your candidate WASM without a public deployment, and lets several
cross-contract calls share local state.

No funds. No public-network writes. No fake implementation of the contracts
under test.

## Example: move a real Aquarius pool price

The included scenario uses the Aquarius XLM/USDC constant-product pool:

`CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE`

Run it with every network proxy deliberately disabled:

```sh
HTTP_PROXY=http://127.0.0.1:9 \
HTTPS_PROXY=http://127.0.0.1:9 \
ALL_PROXY=http://127.0.0.1:9 \
NO_PROXY= \
cargo run --locked --offline --features capture --bin kanatoko -- \
  run aquarius-cp
```

```text
strict Aquarius M3 run: ok
ledger: 63600296
candidate install: local injection (not transaction-faithful deploy)
quote before: 53354881
quote after: 44100959
revert restored quote: 53354881
unknown key fail-closed: true
upstream reads: 0
receipts: 8 (use --format json for detached XDR)
```

That single run:

1. loads a coherent captured mainnet ledger;
2. installs a hash-pinned candidate wrapper and runs its constructor;
3. quotes 1 USDC to XLM, mints a test user 10% of the USDC reserve, and swaps
   through the real captured pool and Stellar Asset Contract WASM;
4. proves the price changed, restores it with `revert`, rejects uncaptured
   state, and reports zero RPC reads.

The full Rust test also inspects authorization trees, events, diagnostics,
state diffs, balances, reserves, and before/after digests:
[tests/m3_aquarius.rs](tests/m3_aquarius.rs).
Add `--format json` to get the same evidence as detached XDR.

## Use fresh mainnet state

Capture starts from the pool address and discovers the ledger keys touched by
the scenario:

```sh
cargo run --locked --features capture --bin kanatoko -- \
  capture aquarius-cp \
  --root CA6PUJLBYKZKUEKLZJMKBZLEKP2OTHANDEOWSFF44FTSYLKQPIICCJBE \
  --bundle /tmp/aquarius.json

cargo run --locked --offline --features capture --bin kanatoko -- \
  run aquarius-cp --fixture /tmp/aquarius.json
```

Capture covers exercised execution paths, not branches that never ran.
Confirmed absence remains distinct from unknown state. Explicitly registered
local candidate contracts are the only intentional exception to unknown-key
rejection.

## Evidence boundary

Kanatoko currently proves contract-functional, state-reproducible Host
behavior. Local candidate installation and recorded/mocked authorization are
explicit test cheats.

It does not claim transaction-faithful deployment, envelope or fee validation,
Stellar Core/consensus behavior, SDEX execution, arbitrary historical replay,
or JSON-RPC daemon compatibility.

## Verify

```sh
cargo test --locked --all-features
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
```

See [MISSION.md](MISSION.md) for product principles and
[the fixture notes](fixtures/mainnet/aquarius-xlm-usdc-cp/README.md) for
capture provenance.
