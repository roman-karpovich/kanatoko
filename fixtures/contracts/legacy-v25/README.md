# Soroban SDK 25 compatibility fixture

This contract proves that Kanatoko's protocol-27 Host can execute candidate
WASM built with Soroban SDK 25. Normal tests consume the committed artifact and
never rebuild it.

From the repository root:

```sh
rustup toolchain install 1.92.0 --profile minimal
rustup target add wasm32v1-none --toolchain 1.92.0

RUSTUP_TOOLCHAIN=1.92.0 \
stellar contract build \
  --manifest-path fixtures/contracts/legacy-v25/Cargo.toml \
  --out-dir fixtures/wasm \
  --optimize \
  --locked
```

Artifact: `fixtures/wasm/kanatoko_legacy_v25_fixture.wasm`

SHA-256: `d601c7569be29b0a52af409ed65425b8c3595db8a83c444fe65dd8294423a879`

The committed artifact is 967 bytes and was built with Stellar CLI 27.0.0
(`5a7c5fe76530bf4248477ac812fc757146b98cc4`) and rustc 1.92.0 for
`wasm32v1-none`. Stellar CLI intentionally rejects Rust 1.91.0 for contract
builds; this fixture toolchain is independent of Kanatoko's Rust 1.91 MSRV.
