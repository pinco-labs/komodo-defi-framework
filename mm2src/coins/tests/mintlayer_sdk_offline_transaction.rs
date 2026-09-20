use coins::mintlayer::{select_spendable_coin_utxos, MintlayerUtxo};
use mintlayer_sdk::crypto::types::*;
use mintlayer_sdk::crypto::{self, Amount, Network, SigHashType, SourceId, TxAdditionalInfo};
use serde_json::json;

const PUBLIC_TEST_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const INPUT_ATOMS: u128 = 100_000_000_000;
const OUTPUT_ATOMS: u128 = 99_999_990_000;
const INCLUSION_HEIGHT: u64 = 700_000;

#[test]
fn build_sign_serialize_and_decode_mainnet_transaction_offline() {
    let network = Network::Mainnet;
    let account_key = crypto::make_default_account_privkey(PUBLIC_TEST_MNEMONIC, network, None).unwrap();
    let spend_key = crypto::make_receiving_address(&account_key, 0).unwrap();
    let public_key = crypto::public_key_from_private_key(&spend_key);
    let address = crypto::pubkey_to_pubkeyhash_address(&public_key, network);
    assert!(address.starts_with("mtc1"));

    let challenge = b"Pinco Labs Mintlayer offline SDK probe";
    let challenge_signature = crypto::sign_challenge(&spend_key, challenge).unwrap();
    assert!(crypto::verify_challenge(&address, network, &challenge_signature, challenge).unwrap());

    let fake_hash = H256::from_slice(&[0x11; 32]);
    let source_id = crypto::encode_outpoint_source_id(fake_hash, SourceId::Transaction);
    let input = crypto::encode_input_for_utxo(source_id, 0);
    let previous_destination = crypto::encode_destination(&address, network).unwrap();
    let previous_output = TxOutput::Transfer(OutputValue::Coin(Amount::from_atoms(INPUT_ATOMS)), previous_destination);
    let output = crypto::encode_output_transfer(Amount::from_atoms(OUTPUT_ATOMS), &address, network).unwrap();

    let inputs = vec![input];
    let outputs = vec![output];
    let input_destinations = [address.as_str()];
    let estimated_size = crypto::estimate_transaction_size(&inputs, &input_destinations, &outputs, network).unwrap();
    assert!(estimated_size > 0);

    let transaction = crypto::encode_transaction(inputs, outputs, 0).unwrap();
    let transaction_id = crypto::transaction_id(&transaction);
    assert_eq!(transaction_id.len(), 64);

    let input_utxos = vec![Some(previous_output)];
    let witness = crypto::encode_witness(
        SigHashType::all(),
        &spend_key,
        &address,
        &transaction,
        &input_utxos,
        0,
        &TxAdditionalInfo::new(),
        INCLUSION_HEIGHT,
        network,
    )
    .unwrap();

    let signed = crypto::encode_signed_transaction(transaction, vec![witness]).unwrap();
    let signed_bytes = signed.encode();
    assert!(!signed_bytes.is_empty());

    let decoded = crypto::decode_signed_transaction_to_json(&signed_bytes, network).unwrap();
    assert!(decoded.to_string().contains(&address));

    println!("network:          mainnet");
    println!("address:          {address}");
    println!("transaction id:   {transaction_id}");
    println!("estimated bytes:  {estimated_size}");
    println!("serialized bytes: {}", signed_bytes.len());
    println!("implicit fee:     {} atoms", INPUT_ATOMS - OUTPUT_ATOMS);
    println!("challenge verify: OK");
    println!("submission:       disabled by construction");
}

#[test]
fn select_api_utxo_build_sign_and_decode_with_change_offline() {
    const INPUT_ATOMS: u128 = 100_000;
    const SEND_ATOMS: u128 = 80_000;
    const FEE_ATOMS: u128 = 10_000;
    const EXPECTED_CHANGE_ATOMS: u128 = 10_000;

    let network = Network::Mainnet;
    let account_key = crypto::make_default_account_privkey(PUBLIC_TEST_MNEMONIC, network, None).unwrap();
    let spend_key = crypto::make_receiving_address(&account_key, 0).unwrap();
    let sender_public_key = crypto::public_key_from_private_key(&spend_key);
    let sender_address = crypto::pubkey_to_pubkeyhash_address(&sender_public_key, network);

    let recipient_key = crypto::make_receiving_address(&account_key, 1).unwrap();
    let recipient_public_key = crypto::public_key_from_private_key(&recipient_key);
    let recipient_address = crypto::pubkey_to_pubkeyhash_address(&recipient_public_key, network);
    drop(recipient_key);

    let source_id = "22".repeat(32);
    let api_utxo: MintlayerUtxo = serde_json::from_value(json!({
        "outpoint": {
            "index": 3,
            "source_id": source_id,
            "source_type": "Transaction"
        },
        "utxo": {
            "destination": sender_address.clone(),
            "type": "Transfer",
            "value": {
                "amount": {
                    "atoms": INPUT_ATOMS.to_string(),
                    "decimal": "0.000001"
                },
                "type": "Coin"
            }
        }
    }))
    .unwrap();

    let selection =
        select_spendable_coin_utxos(&[api_utxo], &sender_address, SEND_ATOMS.checked_add(FEE_ATOMS).unwrap()).unwrap();
    assert_eq!(selection.selected.len(), 1);
    assert_eq!(selection.total_atoms, INPUT_ATOMS);

    let change_atoms = selection
        .total_atoms
        .checked_sub(SEND_ATOMS)
        .and_then(|remaining| remaining.checked_sub(FEE_ATOMS))
        .unwrap();
    assert_eq!(change_atoms, EXPECTED_CHANGE_ATOMS);

    let selected = &selection.selected[0];
    let source_id_bytes = decode_32_byte_hex(&selected.outpoint.source_id);
    let hash = H256::from_slice(&source_id_bytes);
    let encoded_source_id = crypto::encode_outpoint_source_id(hash, SourceId::Transaction);
    let inputs = vec![crypto::encode_input_for_utxo(
        encoded_source_id,
        selected.outpoint.index,
    )];

    let previous_destination = crypto::encode_destination(&sender_address, network).unwrap();
    let input_utxos = vec![Some(TxOutput::Transfer(
        OutputValue::Coin(Amount::from_atoms(selected.atoms)),
        previous_destination,
    ))];

    let outputs = vec![
        crypto::encode_output_transfer(Amount::from_atoms(SEND_ATOMS), &recipient_address, network).unwrap(),
        crypto::encode_output_transfer(Amount::from_atoms(change_atoms), &sender_address, network).unwrap(),
    ];
    let transaction = crypto::encode_transaction(inputs, outputs, 0).unwrap();
    let transaction_id = crypto::transaction_id(&transaction);

    let witness = crypto::encode_witness(
        SigHashType::all(),
        &spend_key,
        &sender_address,
        &transaction,
        &input_utxos,
        0,
        &TxAdditionalInfo::new(),
        INCLUSION_HEIGHT,
        network,
    )
    .unwrap();
    let signed = crypto::encode_signed_transaction(transaction, vec![witness]).unwrap();
    let signed_bytes = signed.encode();
    let decoded = crypto::decode_signed_transaction_to_json(&signed_bytes, network).unwrap();
    let decoded_json = decoded.to_string();

    assert_eq!(transaction_id.len(), 64);
    assert!(decoded_json.contains(&recipient_address));
    assert!(decoded_json.contains(&sender_address));
    assert!(!signed_bytes.is_empty());

    println!("selected inputs:   {}", selection.selected.len());
    println!("selected atoms:    {}", selection.total_atoms);
    println!("send atoms:        {SEND_ATOMS}");
    println!("change atoms:      {change_atoms}");
    println!("fee atoms:         {FEE_ATOMS}");
    println!("transaction id:    {transaction_id}");
    println!("submission:        disabled by construction");
}

fn decode_32_byte_hex(value: &str) -> [u8; 32] {
    assert_eq!(value.len(), 64);
    let mut decoded = [0_u8; 32];
    for (index, byte) in decoded.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&value[offset..offset + 2], 16).unwrap();
    }
    decoded
}
