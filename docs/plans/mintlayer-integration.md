# Mintlayer Integration

## Status

This is an experimental community implementation of native Mintlayer support
for KDF. The current milestone is **wallet-only/read-only on Mintlayer
mainnet**. It is not an official Mintlayer, Gleec, or Komodo release.

Native transaction construction, signing, broadcasting, HTLCs, and atomic
swaps are not implemented yet. Consequently, this milestone does not enable
Mintlayer spot trading.

## Objective

Add Mintlayer to KDF as an independent standalone coin, without treating it as
a Bitcoin-compatible UTXO or Electrum coin. Private keys and deterministic
address derivation remain local to KDF, while public Mintlayer API endpoints
provide blockchain data.

## Implemented capabilities

- `CoinProtocol::MINTLAYER` and the Mintlayer variant in `MmCoinEnum`.
- Typed coin configuration and activation request structures.
- Mainnet, testnet, regtest, and signet network parsing.
- Network and genesis-block validation during activation.
- Chain-tip retrieval through the Mintlayer API web server.
- Deterministic secp256k1 HD key derivation and native Mintlayer addresses.
- Address balance and spendable-UTXO retrieval.
- Correct zero-balance handling for a valid address that has never appeared on
  chain: an address-specific `404 Address not found` from every endpoint is
  mapped to empty address information, while mixed or unrelated failures
  remain errors.
- Validated API endpoints, request timeouts, and ordered endpoint failover.
- Standalone lifecycle integration and legacy `enable` activation.
- Explicit errors for operations that are intentionally unsupported by this
  wallet-only milestone.

## Security model

- KDF derives the wallet identity locally; API servers do not receive private
  keys or seed material.
- API responses are treated as untrusted input and validated before use.
- Activation verifies that the configured API reports the expected network
  genesis block.
- RPC-facing paths avoid panic-based error handling.
- The public runtime does not depend on an SSH tunnel or wallet RPC service.

## Public coin configuration

```json
[
  {
    "coin": "ML",
    "name": "Mintlayer",
    "fname": "Mintlayer",
    "mm2": 1,
    "wallet_only": true,
    "network": "mainnet",
    "decimals": 11,
    "required_confirmations": 2,
    "mature_confirmations": 0,
    "requires_notarization": false,
    "avg_blocktime": 120,
    "genesis_block_id": "2cf01f196066bb6f3a4856deb7999294ff520f633fe48e118e8044390e409870",
    "protocol": {
      "type": "MINTLAYER"
    },
    "links": {}
  }
]
```

## Activation example

The endpoint values below are API roots. KDF normalizes each root and appends
the Mintlayer `/api/v2` prefix internally.

```json
{
  "userpass": "<KDF_RPC_PASSWORD>",
  "method": "enable",
  "coin": "ML",
  "tx_history": false,
  "required_confirmations": 2,
  "client_conf": {
    "api_urls": [
      "https://mintlayer-api.pinco-labs.com",
      "https://api-server.mintlayer.org"
    ]
  }
}
```

The Pinco Labs endpoint is the primary endpoint in this example and the
official Mintlayer API is the fallback. Deployments may supply other compatible
Mintlayer API roots.

## Verified mainnet behavior

The following behavior was verified on 2026-09-19:

- Both public HTTPS endpoints returned the expected mainnet genesis block.
- Both endpoints reported the same chain tip during the comparison test.
- Both endpoints returned the same balance data for the funded KDF address.
- KDF activation derived a native mainnet address and read its `0.1 ML`
  spendable balance.
- A controlled test with an intentionally unreachable primary endpoint fell
  back to the official API and returned the expected balance.
- After restarting the isolated KDF runtime, reactivation preserved the same
  derived address and balance.
- The separate production KDF instance was not modified during these tests.

These checks demonstrate the current read-only milestone; they are not a
substitute for transaction and swap testing in later milestones.

## Current limitations

- No native transaction construction, signing, broadcasting, or withdrawal.
- No transaction-history synchronization or confirmation tracking.
- No Mintlayer HTLC creation, redemption, refund, or watcher logic.
- No atomic swaps or orderbook participation.
- No production-release or upstream-merge approval is implied.

## Requested technical review

Blockchain-specific review is especially welcome for:

- network identifiers, address HRPs, coin types, and HD derivation paths;
- genesis IDs and Mintlayer API endpoint/schema assumptions;
- amount precision, atom/decimal conversion, and UTXO semantics;
- interpretation of `404 Address not found` for unused valid addresses;
- the proposed transaction, HTLC, and confirmation model for later milestones.

## Development roadmap

### Milestone 1 — Protocol scaffold

- [x] Add the Mintlayer module.
- [x] Add `CoinProtocol::MINTLAYER`.
- [x] Add the Mintlayer variant to `MmCoinEnum`.
- [x] Define typed configuration and activation request structures.
- [x] Return explicit errors for unsupported operations.
- [x] Add serialization, protocol parsing, and API schema tests.

### Milestone 2 — Read-only activation

- [x] Implement the Mintlayer API client.
- [x] Validate the network and genesis block.
- [x] Obtain the current block height.
- [x] Derive and display a native Mintlayer address locally.
- [x] Query address balances and spendable UTXOs.
- [x] Implement standalone lifecycle support and legacy activation.
- [x] Add mocked API and validation tests.
- [x] Verify mainnet activation, balance reading, failover, and restart
  persistence.

### Milestone 3 — Native transactions

- [ ] Model transaction inputs, outputs, witnesses, and fees.
- [ ] Implement coin selection and change outputs.
- [ ] Build and sign native Mintlayer transfers locally.
- [ ] Broadcast signed transactions through the API.
- [ ] Track transaction status and confirmations.
- [ ] Add deterministic transaction fixtures and negative tests.

### Milestone 4 — Atomic-swap primitives

- [ ] Map KDF payment, spend, and refund operations to Mintlayer HTLCs.
- [ ] Implement redeem-secret extraction.
- [ ] Implement payment validation and watcher logic.
- [ ] Define lock-time and confirmation behavior.
- [ ] Add recovery and restart tests.

### Milestone 5 — Controlled swap validation

- [ ] Test HTLC creation, redemption, and refund on regtest or testnet.
- [ ] Test controlled KDF maker/taker swaps.
- [ ] Test interruption and recovery scenarios.
- [ ] Test mainnet with minimal amounts only after testnet completion.

### Milestone 6 — Production infrastructure

- [x] Publish a mainnet HTTPS API endpoint.
- [x] Use the official Mintlayer API as a fallback endpoint.
- [x] Add endpoint failover and request timeouts.
- [x] Remove development dependence on SSH tunnels and wallet RPC.
- [x] Document the public coin configuration and activation request.
- [ ] Add automated public-endpoint health monitoring.
- [ ] Deploy an additional independently operated community API endpoint.

## Completion criteria

Full Mintlayer support will require all of the following:

- native transfers pass deterministic and network tests;
- HTLC redeem and refund paths pass controlled tests;
- maker/taker swaps pass interruption and recovery tests;
- security review finds no seed, key, amount, or network-validation issues;
- Mintlayer-specific behavior receives domain review;
- KDF maintainers approve the implementation and production rollout.
