# Mintlayer Integration Plan

## Objective

Add native Mintlayer mainnet support to KDF as an independent
standalone coin, without treating Mintlayer as a Bitcoin-compatible
UTXO or Electrum coin.

The implementation must keep private keys and transaction signing
inside the user's KDF instance. Public Mintlayer APIs provide blockchain
data and transaction broadcasting.

## Architecture

- KDF coin type: `MintlayerCoin`
- KDF protocol: `CoinProtocol::MINTLAYER`
- Activation: `InitStandaloneCoinActivationOps`
- Reference implementation: `SiaCoin`
- Blockchain API: Mintlayer API web server
- Transaction model: native Mintlayer transactions and UTXOs
- Swap primitive: native Mintlayer HTLC
- Signing: local inside KDF
- Public infrastructure: redundant HTTPS Mintlayer API servers
- Development infrastructure: private SSH tunnel and wallet RPC only
  for validation and comparison

## Security requirements

- Never log or serialize mnemonics, seeds or private keys.
- Never require a public KDF user to access node RPC cookies.
- Never perform transaction signing on the public API server.
- Zeroize secret material where applicable.
- Validate addresses, hashes, amounts and timelocks strictly.
- Avoid `unwrap`, `expect` and panic paths in RPC-facing code.
- Keep the existing BLOZ and production KDF implementations unchanged.

## Milestone 1 — Protocol scaffold

- [x] Add the Mintlayer module.
- [x] Add `CoinProtocol::MINTLAYER`.
- [ ] Add `MintlayerCoin` to `MmCoinEnum`.
- [x] Define typed configuration and activation request structures.
- [ ] Compile with unsupported operations returning explicit typed errors.
- [x] Add serialization and protocol parsing tests.
- [x] Define initial typed Mintlayer API response structures and schema tests.

## Milestone 2 — Read-only activation

- [x] Implement Mintlayer API client.
- Validate network and genesis block.
- Obtain current block height.
- Derive and display a Mintlayer address locally.
- Query address balance and UTXOs.
- Implement task-based standalone activation.
- Add mocked API tests.

## Milestone 3 — Native transactions

- Define Mintlayer transaction and outpoint types.
- Implement local transaction serialization.
- Implement local key derivation and signing.
- Implement fee estimation.
- Implement transaction broadcast.
- Implement ordinary withdrawal tests.

## Milestone 4 — HTLC swap operations

- Create native Mintlayer HTLC outputs.
- Validate counterparty HTLC payments.
- Spend an HTLC using the secret.
- Refund an HTLC after its timelock.
- Detect spends and extract the revealed secret.
- Implement required `SwapOps` methods.
- Implement required `WatcherOps` behavior.

## Milestone 5 — Swap testing

- Unit-test serialization, signing and address encoding.
- Test HTLC creation, redemption and refund on regtest or testnet.
- Test controlled KDF maker/taker swaps.
- Test interruption and recovery scenarios.
- Test mainnet with minimal amounts only after testnet completion.

## Milestone 6 — Production infrastructure

- Deploy a second independent Mintlayer API server.
- Publish HTTPS API endpoints with health monitoring.
- Add endpoint failover and timeout handling.
- Remove development dependence on SSH tunnels and wallet RPC.
- Document public coin configuration for Gleec DEX.

## Completion criteria

The integration is complete when a normal KDF user can:

1. activate Mintlayer;
2. derive an address locally;
3. query balance and UTXOs;
4. create and sign transactions locally;
5. broadcast through public Mintlayer infrastructure;
6. execute, redeem, refund and recover an atomic swap;
7. operate without node cookies, SSH tunnels or custodial signing.
