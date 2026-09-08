use serde_derive::{Deserialize, Serialize};
use serde_json::Value as Json;

/// Amount returned by the Mintlayer API.
///
/// `atoms` is kept as a string to avoid precision loss at JSON boundaries.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MintlayerAmount {
    pub atoms: String,
    pub decimal: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MintlayerChainTip {
    pub block_height: u64,
    pub block_id: String,
}

/// Minimal information required to identify the network exposed by an API.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MintlayerGenesisInfo {
    pub block_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MintlayerTokenBalance {
    pub token_id: String,
    pub amount: MintlayerAmount,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MintlayerAddressInfo {
    pub coin_balance: MintlayerAmount,
    pub locked_coin_balance: MintlayerAmount,
    pub transaction_history: Vec<String>,
    pub tokens: Vec<MintlayerTokenBalance>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MintlayerOutPoint {
    pub source_id: String,
    pub index: u32,
    pub source_type: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MintlayerUtxo {
    pub outpoint: MintlayerOutPoint,
    /// Mintlayer outputs are tagged, variant-dependent objects.
    pub utxo: Json,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserialize_genesis_info_ignores_unneeded_fields() {
        let response = json!({
            "block_id":
                "2cf01f196066bb6f3a4856deb7999294ff520f633fe48e118e8044390e409870",
            "genesis_message": "Mintlayer mainnet",
            "timestamp": {
                "timestamp": 1706468400
            },
            "utxos": []
        });

        let genesis: MintlayerGenesisInfo = serde_json::from_value(response).unwrap();

        assert_eq!(
            genesis.block_id,
            "2cf01f196066bb6f3a4856deb7999294ff520f633fe48e118e8044390e409870"
        );
    }

    #[test]
    fn deserialize_chain_tip() {
        let response = json!({
            "block_height": 680824,
            "block_id": "635ce5bab5992e1c6a32cc9983ec4d295b1c89cc05afdf328f73041b9331a8d1"
        });

        let tip: MintlayerChainTip = serde_json::from_value(response.clone()).unwrap();

        assert_eq!(tip.block_height, 680824);
        assert_eq!(serde_json::to_value(tip).unwrap(), response);
    }

    #[test]
    fn deserialize_address_info() {
        let response = json!({
            "coin_balance": {
                "atoms": "6382564000000000",
                "decimal": "63825.64"
            },
            "locked_coin_balance": {
                "atoms": "0",
                "decimal": "0"
            },
            "transaction_history": [
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ],
            "tokens": []
        });

        let info: MintlayerAddressInfo = serde_json::from_value(response.clone()).unwrap();

        assert_eq!(info.coin_balance.decimal, "63825.64");
        assert_eq!(info.locked_coin_balance.atoms, "0");
        assert!(info.tokens.is_empty());
        assert_eq!(serde_json::to_value(info).unwrap(), response);
    }

    #[test]
    fn deserialize_spendable_utxo() {
        let response = json!({
            "outpoint": {
                "source_id":
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "index": 1,
                "source_type": "Transaction"
            },
            "utxo": {
                "destination": "mtc1qexample",
                "type": "Transfer",
                "value": {
                    "amount": {
                        "atoms": "261768000000000",
                        "decimal": "2617.68"
                    },
                    "type": "Coin"
                }
            }
        });

        let utxo: MintlayerUtxo = serde_json::from_value(response.clone()).unwrap();

        assert_eq!(utxo.outpoint.index, 1);
        assert_eq!(utxo.outpoint.source_type, "Transaction");
        assert_eq!(utxo.utxo.get("type").and_then(Json::as_str), Some("Transfer"));
        assert_eq!(serde_json::to_value(utxo).unwrap(), response);
    }
}
