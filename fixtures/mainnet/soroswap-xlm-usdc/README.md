# Soroswap XLM/USDC fixture

This directory contains the Protocol 30 `capture.json` and `auto-capture.json`
execution-driven capture bundles of the Soroswap XLM/USDC pair on Mainnet. The
Protocol 29 bundles they replace are kept as `capture-p29.json` and
`auto-capture-p29.json` for cross-protocol rejection only.
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

The committed `capture.json` is a rootless schema-v2 Protocol 30 bundle. It was
captured read-only from `https://mainnet.sorobanrpc.com` at Mainnet ledger
`65645608` (hash
`e8e37e847f4e4195e7bf956435a14ade62282d249c74a3a33ffde578effe99c8`, closed
2026-11-24T09:54:52Z). It reached a fixed point in two rounds with 11 present
and six RPC-confirmed absent entries, and recorded zero RPC reads during final
replay. The pair, router and USDC admin WASM are unchanged from the Protocol 29
capture.

- Canonical ledger digest: `a52290fd2c79b9d35e453a5547b688a3d0b1041c64e6b56c5a97e948facd971f`
- Inventory digest: `c29d9890dce1fef57ca2d7a12477d310cac6e72d2254c30ea2ddd2058a8a48b9`
- Canonical bundle digest: `a7aa75b73c824327e0968601f8230e0e830d245abc10de138726634d3cc8fa3e`
- `capture.json` SHA-256: `9725501a795ef376cb0f452589123b08e0618dcddb47262e1e8188a7674e2a77`

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
- Mainnet ledger: `65645613`
- Ledger hash:
  `9eb717680b9b9c61f7c146b54385a55cc77a6977e48a4817278d36e2e6d94f03`
- Ledger close time: `2026-11-24T09:55:17Z`
- Protocol: `30`
- Discovery: 2 rounds, 13 present, 4 confirmed absent
- Final replay RPC reads: `0`
- Canonical ledger digest:
  `1917f89bd000978aaa8d5785bc226dc44ce1b3743fc97b6b5ef599c35e53df41`
- Inventory digest:
  `d0c748f912be5161f8a6a0fa7e356544bda57fa3604bd1202e752e75a06e1ae6`
- Canonical bundle digest:
  `a1d06e00505852ffd1c142a302e015ff47954df14643c2ad82d8cb4a78c36b44`
- `auto-capture.json` SHA-256:
  `967cc34318223f3ec4b0b87f4caa1213cb359f55fd7c2d9a0af7380d5a560161`

Refresh it with the ignored read-only test:

```sh
cargo test --locked --all-features --test auto_runner \
  refresh_protocol_30_mainnet_fixture -- --exact --ignored
```

The typed clients are deliberately generated from the different
`kanatoko_soroswap_wrapper.wasm` artifact. The test asserts that its hash is
not the captured pair executable hash, proving the imported file is only an
ABI source and the captured network WASM executes. A second negative test
uses incompatible generated bindings and requires the typed `try_*` call to
return an error rather than execute the local artifact. The acceptance passes
with all HTTP proxies pointed at `127.0.0.1:9`.

## Strict mutable candidate workflow

The strict fork loads the schema-v2 Protocol 30 capture without collapsing
Unknown into confirmed Absent. It locally injects the committed hash-pinned
Soroswap wrapper candidate, whose production WASM calls the captured router and
pair WASM. The acceptance estimates 1 USDC -> XLM, mints a synthetic user 10% of
the captured USDC reserve, previews and exact-gates a wrapper swap, then proves
the quote decreased from `77621199` to `64172267` in the mutated session.

Checkpoint/revert restores the first quote (`77621199`), an uncaptured contract
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

`capture-p29.json` and `auto-capture-p29.json` are the Protocol 29 bundles of
the same scenarios, byte for byte as the 29 line commits them as `capture.json`
and `auto-capture.json`:

- `capture-p29.json`: Mainnet ledger `65389557` (hash
  `197036f155f650596c45c80cc0d3257d69131f7d12e5d91240827f977ec26a4d`), 11
  present and six confirmed-absent entries, SHA-256
  `2fcf68e01a6a95b808ef7b761a8ba2a2610c67c56c61d1faedac3a4015dc7030`.
- `auto-capture-p29.json`: Mainnet ledger `65389638` (hash
  `d0dbb60688faa14ce629178805db2229e3549428faee5cdeedbb1819c1d34df8`), 13
  present and four confirmed-absent entries, SHA-256
  `b3ef94bc07f9d2b8224325faf7c7e9ed16a6265ceeadaf460c424e7c987403cc`.

`tests/protocol_boundary.rs` proves that both, through the loaders, the offline
runners and the CLI, fail closed on the Protocol 30 Host with
`UnsupportedProtocol { found: 29, supported: 30 }`, with and without the
newer-protocol opt-in. Older-protocol rejection also uses
`fixtures/testnet/native-xlm-p28/auto-capture.json`,
`fixtures/testnet/native-xlm-p29/auto-capture.json` and the synthetic bundles
in `fixtures/synthetic/`. No Protocol 27 or 28 Mainnet state is committed.

## Newer-protocol mode on this line

On this line the newer-protocol mode accepts exactly Protocol 31 and executes
it on the Protocol 30 Host. No Protocol 31 network exists yet, so no Protocol
31 capture is committed; the mode is tested with synthetic in-memory Protocol
31 snapshots.
