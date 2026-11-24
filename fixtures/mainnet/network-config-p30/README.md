# Protocol 30 Mainnet network-config fixture

The read-only capture at ledger 65648888 (897dcb29d0e27577ea304586e01000dc56b1cc89d6da92bc70069de4e31e9b81) used https://mainnet.sorobanrpc.com.
It contains 17 present and 11 absent entries, discovered in 2 rounds. Final replay used 0 RPC reads.

Capture SHA-256: `4798c64d88fe8ecc85de9fe9a3d30e5f100d13db9649051ba152f8faa746b0ab`.

The captured network configuration, native XLM instance, signer account and absence proofs supply the local signed-transaction tests. The tests explicitly create the other accounts and deploy the local candidate.

`fee-golden.json` contains four successful public Soroban invocations from ledgers 65648881 and 65648884. Two pay rent, two pay no rent, two are fee bumps, and one records AddressV2 authorization. The rent-paying cases carry both the relevant entry sizes and TTL changes in transaction metadata, so their rent can be computed independently. The four compare nonrefundable, refundable, rent and inclusion fees against charges on Mainnet.

To refresh the capture, run `cargo test --locked --all-features --test strict_transaction refresh_mainnet_network_config_fixture -- --exact --ignored`. Replace the fee samples with transactions from the same network configuration and state-size window.
