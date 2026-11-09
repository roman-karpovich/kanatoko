# Soroswap XLM/USDC fixture

This directory contains the Protocol 29 `capture.json` and `auto-capture.json`
execution-driven capture bundles of the Soroswap XLM/USDC pair on Mainnet.
Local WASM artifacts generate client ABIs only; the normal test suite never
contacts a network.

The scenario names two contracts, the Soroswap router and the XLM/USDC pair:

- Router: `CAG5LRYQ5JVEUI5TEID72EYOVX44TTUJT5BQR2J6J77FH65PCCFAJDDH`
- Pair: `CAM7DY53G63XA4AJRS24Z6VFYAFSSF76C3RZ45BE5YU3FQS5255OOABP`
- Pair WASM SHA-256:
  `18051456816b66f12e773a56f77c5794fac1b1fb7ab6e22d4fad5a412770f73e`
- Router WASM SHA-256:
  `4c3db3ebd2d6a2ab23de1f622eaabb39501539b4611b68622ec4e47f76c4ba07`

Executing the real router discovered the rest of the graph: the XLM and USDC
Stellar Asset Contracts, the pair's SAC balances, the USDC issuer account, and
the USDC admin contract `CCPLJV7AKKFIE4LXFVUWZXDI2HNLEC7U3CQHAMVDLYHQFGMGVTCR4D5W`
with its referenced WASM. The Soroswap factory is not touched during a swap and
is not part of these captures.

## Attribution and licence

The captured pair and router WASM entries are the code deployed on Mainnet by
the Soroswap project. They match the `v1.0` release assets of
[`soroswap/core`](https://github.com/soroswap/core), distributed under the
[Apache License 2.0](https://github.com/soroswap/core/blob/main/LICENSE). The
captured `ContractCode` entries contain those WASM binaries unchanged.

## Execution-driven capture and replay

The capture tool starts with an RPC URL. The scenario names the router and pair
like every other address. During execution Kanatoko automatically discovers
every Host-read or Host-written ledger key, follows each discovered contract
instance to its referenced WASM, then rematerializes the whole set at one
coherent ledger. Confirmed-absent keys are retained separately for strict
unknown-key rejection.

New captures atomically write a rootless schema-v2 bundle containing the ledger
snapshot, Present/Absent inventory, sanitized source origin, provenance, and
canonical digests. The tool immediately loads that file and replays quote ->
mint -> swap -> requote without RPC access.
`fixtures/wasm/kanatoko_soroswap_wrapper.wasm` supplies only the compile-time
ABI of the pair and router methods used by the scenarios; executable network
WASM comes from captured `ContractCode` entries.

The committed `capture.json` is a rootless schema-v2 Protocol 29 bundle. It was
captured read-only from `https://mainnet.sorobanrpc.com` at Mainnet ledger
`65389557` (hash
`197036f155f650596c45c80cc0d3257d69131f7d12e5d91240827f977ec26a4d`).
It reached a fixed point in two rounds with 11 present and six RPC-confirmed
absent entries, and recorded zero RPC reads during final replay.

- Canonical ledger digest: `54b45ed4b3d3037e0882c004629ff7574c822d915f4f51e664f2e5f15b0f3ab5`
- Inventory digest: `091d3f650714f6162f7ce62b299a234356eb0a60bd143ab10f37331b542aaf0b`
- Canonical bundle digest: `ce70e7568c10cafa703abe9690de949c7b4fcf873068729e0a6ec3c15c632620`
- `capture.json` SHA-256: `2fcf68e01a6a95b808ef7b761a8ba2a2610c67c56c61d1faedac3a4015dc7030`

Capture from `https://mainnet.sorobanrpc.com` with:

```sh
cargo run --locked --features capture --bin kanatoko -- \
  capture soroswap-xlm-usdc
```

Replay an existing bundle fully offline with:

```sh
cargo run --locked --offline --features capture --bin kanatoko -- \
  run soroswap-xlm-usdc --format text
```

## One-scenario automatic capture

`tests/auto_runner.rs` contains one Rust body for discovery and strict replay.
It mixes generated pair and router clients with dynamic SAC and router
invocations in one `Env`, creates a deterministic local G-address, explicitly
funds it, establishes its real SAC trustline, mints it 10% of the USDC reserve,
swaps through the captured graph, and proves the 1 USDC -> XLM quote moved. The
same scenario reads the real Mainnet G-account
`GAI2BF5I2G77J653SVF5ONB77SURJSD225MLZZOBWWBR3WU3YEPVIEO2` through both XLM and
USDC SACs, explicitly funds a separate local sender, then transfers one stroop
to the real account's M-address and verifies the multiplexing ID in the emitted
event.

The committed `auto-capture.json` was created by the runner itself on the
first online execution; no separate capture scenario or manifest was written.

- Bundle schema: `2` (no root address)
- Mainnet ledger: `65389638`
- Ledger hash:
  `d0dbb60688faa14ce629178805db2229e3549428faee5cdeedbb1819c1d34df8`
- Protocol: `29`
- Discovery: 2 rounds, 13 present, 4 confirmed absent
- Final replay RPC reads: `0`
- Canonical ledger digest:
  `3afb9bbae36e470dc932732b59e59cc2a1593fa08d03ca1ff2286a5376745d58`
- Inventory digest:
  `1ea365cb97074b8a86cfa0c8a67cb5325fb02c1614dafbbbed471ee5cfd2e4de`
- Canonical bundle digest:
  `b9299ccaa4930f753c3982a5c9a8ac2836158ab67714719419b0cf26930ba1f4`
- `auto-capture.json` SHA-256:
  `b3ef94bc07f9d2b8224325faf7c7e9ed16a6265ceeadaf460c424e7c987403cc`

Refresh it with the ignored read-only test:

```sh
cargo test --locked --all-features --test auto_runner \
  refresh_protocol_29_mainnet_fixture -- --exact --ignored
```

The typed clients are deliberately generated from the different
`kanatoko_soroswap_wrapper.wasm` artifact. The test asserts that its hash is
not the captured pair executable hash, proving the imported file is only an
ABI source and the captured network WASM executes. A second negative test
uses incompatible generated bindings and requires the typed `try_*` call to
return an error rather than execute the local artifact. The acceptance passes
with all HTTP proxies pointed at `127.0.0.1:9`.

## Strict mutable candidate workflow

The strict fork loads the schema-v2 Protocol 29 capture without collapsing
Unknown into confirmed Absent. It locally injects the committed hash-pinned
Soroswap wrapper candidate, whose production WASM calls the captured router and
pair WASM. The acceptance estimates 1 USDC -> XLM, mints a synthetic user 10% of
the captured USDC reserve, previews and exact-gates a wrapper swap, then proves
the quote decreased from `67870832` to `56106992` in the mutated session.

Checkpoint/revert restores the first quote (`67870832`), an uncaptured contract
key fails closed after the mutations and after revert, and every receipt plus
the fork reports zero upstream reads. JSON output exposes detached XDR for
results, exact auth trees, events, diagnostics, and ledger diffs.

The observed synthetic-user authorization tree is rooted at the local wrapper
`swap_exact_tokens_for_tokens`, continues through the captured router call, and
ends at the captured USDC SAC `transfer`. Record mode mock-satisfies discovery;
mock-exact runs in an isolated recording child and commits only on
byte-for-byte detached tree equality. This is not signature or
transaction-faithful deployment evidence. Host-generated recording nonces are
treated as mocked-auth scaffolding and are not committed. The nonce exception
is not active in enforce mode, where an uncaptured anti-replay key remains
Unknown and fails closed.

The live USDC admin is a contract address, so the local USDC mint runs in
recording mode: SDK `MockAuth` would otherwise register its test auth contract
at that address and replace imported state. The swap authorization tree is
exact. Mainnet capture works only while the network runs the Protocol of the
selected line; the two bundles here can be refreshed with the documented
read-only commands.

## Older-protocol rejection evidence

Older-protocol rejection uses
`fixtures/testnet/native-xlm-p28/auto-capture.json` and the synthetic bundles in
`fixtures/synthetic/`. No Protocol 27 or 28 Mainnet state is committed.

## Newer-protocol mode on this line

On this line the newer-protocol mode accepts exactly Protocol 30 and executes
it on the Protocol 29 Host. No Protocol 30 network exists yet, so no Protocol 30
capture is committed; the mode is tested with synthetic in-memory Protocol 30
snapshots.
