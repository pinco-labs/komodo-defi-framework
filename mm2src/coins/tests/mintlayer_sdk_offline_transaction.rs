use mintlayer_sdk::crypto::types::*;
use mintlayer_sdk::crypto::{self, Amount, Network, SigHashType, SourceId, TxAdditionalInfo};

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
