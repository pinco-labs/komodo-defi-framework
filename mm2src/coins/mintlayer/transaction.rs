use crate::mintlayer::{
    select_spendable_coin_utxos, MintlayerFeeError, MintlayerFeeRate, MintlayerUtxo, MintlayerUtxoSelection,
    MintlayerUtxoSelectionError,
};
use mintlayer_sdk::crypto::types::*;
use mintlayer_sdk::crypto::{self, Amount, Network, SigHashType, SourceId, TxAdditionalInfo};
use std::convert::TryInto;
use thiserror::Error;

const MAX_FEE_ITERATIONS: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MintlayerSignedTransactionPlan {
    pub transaction_id: String,
    pub signed_bytes: Vec<u8>,
    pub selected_inputs: usize,
    pub selected_atoms: u128,
    pub send_atoms: u128,
    pub change_atoms: u128,
    pub fee_atoms: u128,
    pub minimum_fee_atoms: u128,
    pub serialized_bytes: usize,
    pub iterations: usize,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum MintlayerTransactionPlanError {
    #[error("Mintlayer send amount must be greater than zero")]
    ZeroSendAmount,
    #[error("Mintlayer sender address does not match the signing key")]
    SenderKeyMismatch,
    #[error("Mintlayer amount calculation overflow")]
    AmountOverflow,
    #[error("Unsupported Mintlayer outpoint source type '{0}'")]
    UnsupportedSourceType(String),
    #[error("Invalid Mintlayer source id '{0}'")]
    InvalidSourceId(String),
    #[error("Mintlayer UTXO selection failed: {0}")]
    UtxoSelection(#[from] MintlayerUtxoSelectionError),
    #[error("Mintlayer fee calculation failed: {0}")]
    Fee(#[from] MintlayerFeeError),
    #[error("Mintlayer SDK transaction operation failed: {0}")]
    Sdk(String),
    #[error("Mintlayer canonical fee convergence entered a cycle")]
    FeeConvergenceCycle,
    #[error("Mintlayer canonical fee did not converge within {0} iterations")]
    FeeDidNotConverge(usize),
    #[error("Mintlayer transaction amount conservation failed")]
    AmountConservation,
}

/// Builds and signs a native Mintlayer transfer entirely in memory.
///
/// This function has no API client, transport or submission capability. The
/// caller supplies already-fetched UTXOs and the fee rate; the result is a
/// signed byte vector that remains offline until another layer explicitly
/// decides how to handle it.
pub fn plan_signed_transaction_offline(
    utxos: &[MintlayerUtxo],
    sender_address: &str,
    recipient_address: &str,
    send_atoms: u128,
    fee_rate: MintlayerFeeRate,
    spend_key: &PrivateKey,
    inclusion_height: u64,
    network: Network,
) -> Result<MintlayerSignedTransactionPlan, MintlayerTransactionPlanError> {
    if send_atoms == 0 {
        return Err(MintlayerTransactionPlanError::ZeroSendAmount);
    }

    let signer_public_key = crypto::public_key_from_private_key(spend_key);
    let signer_address = crypto::pubkey_to_pubkeyhash_address(&signer_public_key, network);
    if signer_address != sender_address {
        return Err(MintlayerTransactionPlanError::SenderKeyMismatch);
    }

    crypto::encode_destination(sender_address, network).map_err(sdk_error)?;
    crypto::encode_destination(recipient_address, network).map_err(sdk_error)?;

    let mut fee_guess_atoms = 0_u128;
    let mut previous_non_final_state = None;

    for iteration in 1..=MAX_FEE_ITERATIONS {
        let required_atoms = send_atoms
            .checked_add(fee_guess_atoms)
            .ok_or(MintlayerTransactionPlanError::AmountOverflow)?;
        let selection = select_spendable_coin_utxos(utxos, sender_address, required_atoms)?;
        let available_fee_atoms = selection
            .total_atoms
            .checked_sub(send_atoms)
            .ok_or(MintlayerTransactionPlanError::AmountConservation)?;
        let change_atoms = available_fee_atoms
            .checked_sub(fee_guess_atoms)
            .ok_or(MintlayerTransactionPlanError::AmountConservation)?;
        let candidate = build_signed_candidate(
            &selection,
            sender_address,
            recipient_address,
            send_atoms,
            change_atoms,
            spend_key,
            inclusion_height,
            network,
        )?;
        let canonical_fee_atoms = fee_rate.compute_fee_atoms(candidate.signed_bytes.len())?;

        if canonical_fee_atoms == fee_guess_atoms {
            return finalize_plan(
                candidate,
                selection,
                send_atoms,
                change_atoms,
                canonical_fee_atoms,
                canonical_fee_atoms,
                iteration,
            );
        }

        if change_atoms > 0 && available_fee_atoms <= canonical_fee_atoms {
            let no_change = build_signed_candidate(
                &selection,
                sender_address,
                recipient_address,
                send_atoms,
                0,
                spend_key,
                inclusion_height,
                network,
            )?;
            let no_change_minimum_fee = fee_rate.compute_fee_atoms(no_change.signed_bytes.len())?;

            if available_fee_atoms >= no_change_minimum_fee {
                return finalize_plan(
                    no_change,
                    selection,
                    send_atoms,
                    0,
                    available_fee_atoms,
                    no_change_minimum_fee,
                    iteration,
                );
            }
        }

        let state = (
            selection.selected.len(),
            change_atoms > 0,
            candidate.signed_bytes.len(),
            canonical_fee_atoms,
        );
        if previous_non_final_state == Some(state) {
            return Err(MintlayerTransactionPlanError::FeeConvergenceCycle);
        }
        previous_non_final_state = Some(state);
        fee_guess_atoms = canonical_fee_atoms;
    }

    Err(MintlayerTransactionPlanError::FeeDidNotConverge(MAX_FEE_ITERATIONS))
}

struct SignedCandidate {
    transaction_id: String,
    signed_bytes: Vec<u8>,
}

#[allow(clippy::too_many_arguments)]
fn build_signed_candidate(
    selection: &MintlayerUtxoSelection,
    sender_address: &str,
    recipient_address: &str,
    send_atoms: u128,
    change_atoms: u128,
    spend_key: &PrivateKey,
    inclusion_height: u64,
    network: Network,
) -> Result<SignedCandidate, MintlayerTransactionPlanError> {
    let inputs = selection
        .selected
        .iter()
        .map(|selected| {
            let source_id_bytes = decode_source_id(&selected.outpoint.source_id)?;
            let hash = H256::from_slice(&source_id_bytes);
            let source_type = match selected.outpoint.source_type.as_str() {
                "Transaction" => SourceId::Transaction,
                "BlockReward" => SourceId::BlockReward,
                other => return Err(MintlayerTransactionPlanError::UnsupportedSourceType(other.to_owned())),
            };
            let source_id = crypto::encode_outpoint_source_id(hash, source_type);
            Ok(crypto::encode_input_for_utxo(source_id, selected.outpoint.index))
        })
        .collect::<Result<Vec<_>, MintlayerTransactionPlanError>>()?;

    let previous_destination = crypto::encode_destination(sender_address, network).map_err(sdk_error)?;
    let input_utxos = selection
        .selected
        .iter()
        .map(|selected| {
            Some(TxOutput::Transfer(
                OutputValue::Coin(Amount::from_atoms(selected.atoms)),
                previous_destination.clone(),
            ))
        })
        .collect::<Vec<_>>();

    let mut outputs = vec![
        crypto::encode_output_transfer(Amount::from_atoms(send_atoms), recipient_address, network)
            .map_err(sdk_error)?,
    ];
    if change_atoms > 0 {
        outputs.push(
            crypto::encode_output_transfer(Amount::from_atoms(change_atoms), sender_address, network)
                .map_err(sdk_error)?,
        );
    }

    let transaction = crypto::encode_transaction(inputs, outputs, 0).map_err(sdk_error)?;
    let transaction_id = crypto::transaction_id(&transaction);
    let mut witnesses = Vec::with_capacity(input_utxos.len());
    for input_index in 0..input_utxos.len() {
        witnesses.push(
            crypto::encode_witness(
                SigHashType::all(),
                spend_key,
                sender_address,
                &transaction,
                &input_utxos,
                input_index,
                &TxAdditionalInfo::new(),
                inclusion_height,
                network,
            )
            .map_err(sdk_error)?,
        );
    }

    let signed = crypto::encode_signed_transaction(transaction, witnesses).map_err(sdk_error)?;
    Ok(SignedCandidate {
        transaction_id,
        signed_bytes: signed.encode(),
    })
}

fn finalize_plan(
    candidate: SignedCandidate,
    selection: MintlayerUtxoSelection,
    send_atoms: u128,
    change_atoms: u128,
    fee_atoms: u128,
    minimum_fee_atoms: u128,
    iterations: usize,
) -> Result<MintlayerSignedTransactionPlan, MintlayerTransactionPlanError> {
    let conserved = send_atoms
        .checked_add(change_atoms)
        .and_then(|value| value.checked_add(fee_atoms))
        .ok_or(MintlayerTransactionPlanError::AmountOverflow)?;
    if conserved != selection.total_atoms {
        return Err(MintlayerTransactionPlanError::AmountConservation);
    }

    let serialized_bytes = candidate.signed_bytes.len();
    Ok(MintlayerSignedTransactionPlan {
        transaction_id: candidate.transaction_id,
        signed_bytes: candidate.signed_bytes,
        selected_inputs: selection.selected.len(),
        selected_atoms: selection.total_atoms,
        send_atoms,
        change_atoms,
        fee_atoms,
        minimum_fee_atoms,
        serialized_bytes,
        iterations,
    })
}

fn decode_source_id(value: &str) -> Result<[u8; 32], MintlayerTransactionPlanError> {
    let decoded = hex::decode(value).map_err(|_| MintlayerTransactionPlanError::InvalidSourceId(value.to_owned()))?;
    decoded
        .try_into()
        .map_err(|_| MintlayerTransactionPlanError::InvalidSourceId(value.to_owned()))
}

fn sdk_error(error: impl ToString) -> MintlayerTransactionPlanError {
    MintlayerTransactionPlanError::Sdk(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PUBLIC_TEST_MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const FEE_RATE_ATOMS_PER_KB: u128 = 100_000_000_000;
    const INCLUSION_HEIGHT: u64 = 700_000;

    #[test]
    fn plans_and_signs_canonical_transaction_without_submission() {
        let network = Network::Mainnet;
        let account_key = crypto::make_default_account_privkey(PUBLIC_TEST_MNEMONIC, network, None).unwrap();
        let spend_key = crypto::make_receiving_address(&account_key, 0).unwrap();
        let sender_address = address_for_key(&spend_key, network);
        let recipient_key = crypto::make_receiving_address(&account_key, 1).unwrap();
        let recipient_address = address_for_key(&recipient_key, network);
        let utxos = vec![
            coin_utxo("11".repeat(32), 0, 60_000_000_000, "0.6", &sender_address),
            coin_utxo("22".repeat(32), 1, 40_000_000_000, "0.4", &sender_address),
        ];

        let plan = plan_signed_transaction_offline(
            &utxos,
            &sender_address,
            &recipient_address,
            50_000_000_000,
            MintlayerFeeRate::from_atoms_per_kb(FEE_RATE_ATOMS_PER_KB),
            &spend_key,
            INCLUSION_HEIGHT,
            network,
        )
        .unwrap();

        assert_eq!(plan.selected_inputs, 2);
        assert_eq!(plan.selected_atoms, 100_000_000_000);
        assert_eq!(plan.change_atoms, 15_500_000_000);
        assert_eq!(plan.fee_atoms, 34_500_000_000);
        assert_eq!(plan.minimum_fee_atoms, plan.fee_atoms);
        assert_eq!(plan.serialized_bytes, 345);
        assert_eq!(
            plan.transaction_id,
            "df383086aebaf0f809b8e1ce55fb9e2faafb4aef3716cd1af32954944d96720c"
        );
        assert_eq!(
            plan.send_atoms + plan.change_atoms + plan.fee_atoms,
            plan.selected_atoms
        );
        assert!(plan.iterations >= 2);
        assert!(!plan.signed_bytes.is_empty());
    }

    #[test]
    fn absorbs_small_residue_without_creating_change() {
        let network = Network::Mainnet;
        let account_key = crypto::make_default_account_privkey(PUBLIC_TEST_MNEMONIC, network, None).unwrap();
        let spend_key = crypto::make_receiving_address(&account_key, 0).unwrap();
        let sender_address = address_for_key(&spend_key, network);
        let recipient_key = crypto::make_receiving_address(&account_key, 1).unwrap();
        let recipient_address = address_for_key(&recipient_key, network);
        let input_atoms = 68_700_000_000;
        let utxos = vec![coin_utxo("33".repeat(32), 0, input_atoms, "0.687", &sender_address)];

        let plan = plan_signed_transaction_offline(
            &utxos,
            &sender_address,
            &recipient_address,
            50_000_000_000,
            MintlayerFeeRate::from_atoms_per_kb(FEE_RATE_ATOMS_PER_KB),
            &spend_key,
            INCLUSION_HEIGHT,
            network,
        )
        .unwrap();

        assert_eq!(plan.change_atoms, 0);
        assert_eq!(plan.serialized_bytes, 175);
        assert_eq!(plan.minimum_fee_atoms, 17_500_000_000);
        assert_eq!(plan.fee_atoms, 18_700_000_000);
        assert_eq!(plan.send_atoms + plan.fee_atoms, input_atoms);
    }

    #[test]
    fn rejects_zero_amount_and_mismatched_key() {
        let network = Network::Mainnet;
        let account_key = crypto::make_default_account_privkey(PUBLIC_TEST_MNEMONIC, network, None).unwrap();
        let spend_key = crypto::make_receiving_address(&account_key, 0).unwrap();
        let other_key = crypto::make_receiving_address(&account_key, 1).unwrap();
        let sender_address = address_for_key(&spend_key, network);
        let recipient_address = address_for_key(&other_key, network);
        let fee_rate = MintlayerFeeRate::from_atoms_per_kb(FEE_RATE_ATOMS_PER_KB);

        assert_eq!(
            plan_signed_transaction_offline(
                &[],
                &sender_address,
                &recipient_address,
                0,
                fee_rate,
                &spend_key,
                INCLUSION_HEIGHT,
                network,
            ),
            Err(MintlayerTransactionPlanError::ZeroSendAmount)
        );
        assert_eq!(
            plan_signed_transaction_offline(
                &[],
                &sender_address,
                &recipient_address,
                1,
                fee_rate,
                &other_key,
                INCLUSION_HEIGHT,
                network,
            ),
            Err(MintlayerTransactionPlanError::SenderKeyMismatch)
        );
    }

    fn address_for_key(key: &PrivateKey, network: Network) -> String {
        let public_key = crypto::public_key_from_private_key(key);
        crypto::pubkey_to_pubkeyhash_address(&public_key, network)
    }

    fn coin_utxo(source_id: String, index: u32, atoms: u128, decimal: &str, destination: &str) -> MintlayerUtxo {
        serde_json::from_value(json!({
            "outpoint": {
                "index": index,
                "source_id": source_id,
                "source_type": "Transaction"
            },
            "utxo": {
                "destination": destination,
                "type": "Transfer",
                "value": {
                    "amount": { "atoms": atoms.to_string(), "decimal": decimal },
                    "type": "Coin"
                }
            }
        }))
        .unwrap()
    }
}
