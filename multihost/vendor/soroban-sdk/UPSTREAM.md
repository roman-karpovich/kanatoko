# Stellar SDK 29 compatibility sources

The three directories contain the Rust sources from the crates.io release of Stellar SDK 29.0.1. They share upstream commit e1671a0cb58d4b8c0bb8de115caf080ca01676e8 and the accompanying Apache 2.0 license. The SDK build script is also from that release. Only the Cargo manifests differ. They select the published 29.0.2 environment crates, point at the sibling SDK components and omit upstream test targets and development dependencies.

| Crate | SHA-256 of the upstream 29.0.1 .crate archive |
| --- | --- |
| soroban-sdk | 6a3b97a76c88f413859bb670903b63af54d7ddb03303a5bd585fee8b9f2a3d8f |
| soroban-sdk-macros | c4ad070a31d327a4cbe4eca2ead25eb1905ae022ec1d0f5ebd216e1fe00f064e |
| soroban-ledger-snapshot | 76086b30da4ad888d991f40ca28215cff394132aeaacecfa42e88d4e2cbcfd2b |

Host 29.0.2 depends on soroban-wasmi 29.0.0. Cargo can resolve it alongside the older interpreter required by SDK 27. SDK 29.0.1 pins Host 29.0.1, and the snapshot and macro crates also pin environment versions. These local manifests keep the 29 runtime on a consistent patch. If upstream publishes an SDK 29 release with compatible pins, switch all three components together and rerun the consumer tests on both protocols.

[Stellar SDK source](https://github.com/stellar/rs-soroban-sdk/tree/e1671a0cb58d4b8c0bb8de115caf080ca01676e8)

This package is published by Kanatoko as kanatoko-sdk29. The upstream library name and Rust sources are preserved.
