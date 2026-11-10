# Protocol 30 standalone fixture

When this fixture was made, no public Stellar network ran Protocol 30: testnet
and Mainnet still reported Protocol 29. `capture.json` is therefore a read-only
capture of a local standalone network started from
`stellar/quickstart:nightly-next`, whose stellar-core (`v30.0.0-22-gb8c4f12d6`)
upgrades the network to Protocol 30 at startup.

- network passphrase: `Standalone Network ; February 2017`
- RPC origin: `http://localhost:8000`
- quickstart image:
  `stellar/quickstart@sha256:79ca9a343523a6fef81685e5bf1630b569a700627a81274f27ced0332fdb6ace`
- protocol: 30
- ledger: `318`
- ledger hash: `31f741303e186fde9fba0fd54fb6e5d161abd9fcac1effab3935c94d263859d9`
- captured on: 2026-11-02
- discovery: 2 rounds, 5 present, 2 confirmed absent
- ledger digest: `046c76b43dd29f19683c865d6116ea14c41d957dc6c3f2c7e281d8d3b0509998`
- inventory digest: `2ae27c8eae93cdce78b0f689ff1cbe25836827a80ca48cfa894e3ac757215ca5`
- canonical bundle digest: `1e26a9338ed5c5e19b4b450384bcf3999f6f17c2fa16bb064c6b130e984acb76`
- SHA-256: `eeb6715435fc546ed38dea22ec1793e2ddbe6cbe6aa519d2db50fbc7e3226d92`

The five present entries are:

- the native XLM Stellar Asset Contract instance
  `CDMLFMKMMD7MWZP3FKUBZPVHTUEDLSX4BYGYKH4GCESXYHS3IHQ4EIG4`;
- the deployer account
  `GDG2BXH36BVGBOF62IXSAIZXKJ62CAIWHEFQOAA5ZGIEOMXQDYSPZ7VT`;
- the stateful fixture contract
  `CCNHIRURTVMTX65DOASMBGJSQZLZMFGRO7QBHMBDSFPGEQ7AWH6VMTH5`: its instance,
  its persistent `value` entry (42), and its ContractCode entry, which is the
  committed `fixtures/wasm/kanatoko_stateful_fixture.wasm` (SHA-256
  `0156a9a840e4147732fcf0479846220840ae4b4281c58354aa31cf70daf6b2ea`, built
  with `soroban-sdk` 28) together with the Protocol 30 cost inputs recorded at
  upload.

The two confirmed-absent entries are the native XLM balance of the stateful
contract, which the scenario credits locally, and the deployer's temporary
authorization nonce key touched by the Host's recording authorization.

The network state was produced by real Protocol 30 transactions, in ledgers
181 to 184: deploy the native XLM Stellar Asset Contract, upload the stateful
WASM, create the contract with constructor argument 41, and invoke
`increment(1)`. The scenario in `tests/standalone_p30.rs` then reads XLM
`decimals`, executes the captured stateful contract (`get` = 42, then
`increment(1)` = 43 locally), and transfers one stroop of native XLM from the
deployer to the muxed contract address (CAP-84) of the stateful contract with
id 30. The offline tests replay that scenario with zero RPC reads and repeat
the contract calls and the muxed transfer on a strict fork, checking the
`to_muxed_id` in the transfer event.

## Refresh

The deployer key and the contract salt are derived from fixed labels, so the
addresses above are the same on every fresh standalone network. A refresh
captures a different ledger, so the ledger, hash, and digest constants in
`tests/standalone_p30.rs` and this file must then be updated.

1. Start the network:

   ```sh
   docker run -d --rm -p 127.0.0.1:8000:8000 --name kanatoko-p30 \
     stellar/quickstart:nightly-next --local
   ```

2. Once `getNetwork` reports protocol 30, prepare the ledger state. The setup
   tool in `setup/` only talks to the given local RPC and friendbot:

   ```sh
   cargo run --locked --manifest-path \
     fixtures/standalone/native-xlm-stateful-p30/setup/Cargo.toml -- \
     http://localhost:8000/rpc http://localhost:8000/friendbot \
     fixtures/wasm/kanatoko_stateful_fixture.wasm
   ```

3. Capture, read-only:

   ```sh
   cargo test --locked --all-features --test standalone_p30 \
     refresh_protocol_30_standalone_fixture -- --exact --ignored --nocapture
   ```

4. Stop the network with `docker stop kanatoko-p30`.

Verify offline replay with network access trapped:

```sh
HTTP_PROXY=http://127.0.0.1:9 \
HTTPS_PROXY=http://127.0.0.1:9 \
ALL_PROXY=http://127.0.0.1:9 \
cargo test --locked --all-features --test standalone_p30
```

Replace this fixture with a public-network capture once testnet or Mainnet
moves to Protocol 30.
