use crate::coin_errors::{AddressFromPubkeyError, MyAddressError};
use crate::hd_wallet::HDAddressSelector;
use crate::mintlayer::address::validate_mintlayer_address;
use crate::mintlayer::{
    broadcast_signed_transaction_hex, build_mintlayer_htlc_output, canonical_transaction_id_from_signed_bytes,
    mintlayer_address_from_compressed_public_key, mintlayer_derivation_path, plan_signed_output_offline,
    plan_signed_transaction_offline, sdk_private_key_from_kdf_key_pair, MintlayerActivationRequest,
    MintlayerAddressInfo, MintlayerApiClient, MintlayerApiError, MintlayerChainTip, MintlayerCoinConf,
    MintlayerNetwork, MintlayerNodeClientConfig, MintlayerSignedTransactionPlan, MintlayerTransaction, MintlayerUtxo,
};
use crate::utxo::UtxoFeeDetails;
use crate::{
    BalanceError, BalanceFut, CoinBalance, ConfirmPaymentInput, DerivationMethodResponse, MarketCoinOps,
    PrivKeyBuildPolicy, SignatureError, SignatureResult, TransactionData, TransactionDetails, TransactionEnum,
    TransactionErr, TransactionResult, TxFeeDetails, TxMarshalingErr, UnexpectedDerivationMethod, VerificationError,
    VerificationResult, WaitForHTLCTxSpendArgs,
};
use crate::{
    CheckIfMyPaymentSentArgs, DexFee, FeeApproxStage, FoundSwapTxSpend, HistorySyncState, MmCoin,
    NegotiateSwapContractAddrErr, RawTransactionError, RawTransactionFut, RawTransactionRequest, RefundPaymentArgs,
    SearchForSwapTxSpendInput, SendPaymentArgs, SpendPaymentArgs, SwapOps, TradeFee, TradePreimageError,
    TradePreimageFut, TradePreimageResult, TradePreimageValue, ValidateAddressResult, ValidateFeeArgs,
    ValidateOtherPubKeyErr, ValidatePaymentError, ValidatePaymentInput, ValidatePaymentResult, WatcherOps, WeakSpawner,
    WithdrawError, WithdrawFut, WithdrawRequest,
};
use async_trait::async_trait;
use common::executor::AbortedError;
use common::now_sec;
use rpc::v1::types::Bytes as BytesJson;
use serde_json::Value as Json;

use common::executor::abortable_queue::AbortableQueue;
use common::executor::AbortableSystem;
use crypto::privkey::key_pair_from_secret;
use crypto::Bip44Chain;
use derive_more::Display;
use futures::compat::Future01CompatExt;
use futures::{FutureExt, TryFutureExt};
use futures01::Future;
use keys::KeyPair;
use mintlayer_sdk::crypto::Network as SdkNetwork;
use mintlayer_sdk::node::Client as MintlayerNodeClient;
use mm2_core::mm_ctx::MmArc;
use mm2_err_handle::prelude::*;
use mm2_number::{BigDecimal, BigInt, MmNumber};
use rpc::v1::types::H264 as H264Json;
use std::collections::HashSet;
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use url::{Host, Url};

pub const MINTLAYER_DECIMALS: u8 = 11;

#[derive(Debug, Display, PartialEq)]
pub enum MintlayerCoinBuildError {
    EmptyTicker,
    #[display(fmt = "Invalid Mintlayer decimals: expected {}, found {}", expected, actual)]
    InvalidDecimals {
        expected: u8,
        actual: u8,
    },
    InvalidGenesisBlockId,
    MissingApiUrls,
    #[display(fmt = "Invalid Mintlayer API URL at index {}", index)]
    InvalidApiUrl {
        index: usize,
    },
    #[display(fmt = "Unsupported Mintlayer API URL scheme at index {}", index)]
    UnsupportedApiUrlScheme {
        index: usize,
    },
    #[display(fmt = "Credentials are not allowed in Mintlayer API URL at index {}", index)]
    ApiUrlCredentialsNotAllowed {
        index: usize,
    },
    #[display(fmt = "Plain HTTP is allowed only for loopback Mintlayer API URLs; index {}", index)]
    InsecureRemoteApiUrl {
        index: usize,
    },
    #[display(fmt = "Duplicate Mintlayer API URL at index {}", index)]
    DuplicateApiUrl {
        index: usize,
    },
    InvalidNodeRpcUrl,
    NodeRpcUrlCredentialsNotAllowed,
    UnsupportedNodeRpcUrlScheme,
    RemoteNodeRpcUrlNotAllowed,
    InvalidRequiredConfirmations,
    #[display(fmt = "Failed to derive Mintlayer key: {}", _0)]
    KeyDerivation(String),
    #[display(fmt = "Failed to derive Mintlayer address: {}", _0)]
    AddressDerivation(String),
    #[display(fmt = "Unsupported Mintlayer private key policy: {}", policy)]
    UnsupportedPrivKeyPolicy {
        policy: &'static str,
    },
    #[display(fmt = "Failed to create Mintlayer abortable subsystem: {}", _0)]
    AbortableSystem(String),
}

#[derive(Debug, Display)]
pub enum MintlayerNetworkValidationError {
    #[display(fmt = "Mintlayer API request failed: {}", _0)]
    Api(MintlayerApiError),
    #[display(fmt = "Mintlayer API returned invalid genesis block ID '{}'", actual)]
    InvalidGenesisBlockId { actual: String },
    #[display(
        fmt = "Unexpected Mintlayer genesis block ID: expected '{}', found '{}'",
        expected,
        actual
    )]
    UnexpectedGenesisBlockId { expected: String, actual: String },
}

impl From<MintlayerApiError> for MintlayerNetworkValidationError {
    fn from(error: MintlayerApiError) -> Self {
        MintlayerNetworkValidationError::Api(error)
    }
}

#[derive(Clone, Debug)]
pub struct MintlayerCoin(Arc<MintlayerCoinImpl>);

impl Deref for MintlayerCoin {
    type Target = MintlayerCoinImpl;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct MintlayerCoinImpl {
    conf: MintlayerCoinConf,
    api_urls: Vec<Url>,
    api_client: MintlayerApiClient,
    node_conf: Option<MintlayerNodeClientConfig>,
    tx_history: bool,
    required_confirmations: AtomicU64,
    key_pair: KeyPair,
    address: String,
    derivation_method: DerivationMethodResponse,
    pub abortable_system: Arc<AbortableQueue>,
}

impl fmt::Debug for MintlayerCoinImpl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MintlayerCoinImpl")
            .field("conf", &self.conf)
            .field("api_urls", &self.api_urls)
            .field("node_conf", &self.node_conf)
            .field("tx_history", &self.tx_history)
            .field(
                "required_confirmations",
                &self.required_confirmations.load(Ordering::Relaxed),
            )
            .field("key_pair", &"<redacted>")
            .field("public_key", &hex::encode(self.key_pair.public_slice()))
            .field("address", &self.address)
            .field("derivation_method", &self.derivation_method)
            .finish()
    }
}

impl MintlayerCoin {
    pub fn new(
        ctx: &MmArc,
        conf: MintlayerCoinConf,
        request: MintlayerActivationRequest,
        priv_key_build_policy: PrivKeyBuildPolicy,
    ) -> Result<Self, MintlayerCoinBuildError> {
        if conf.ticker.trim().is_empty() {
            return Err(MintlayerCoinBuildError::EmptyTicker);
        }

        if conf.decimals != MINTLAYER_DECIMALS {
            return Err(MintlayerCoinBuildError::InvalidDecimals {
                expected: MINTLAYER_DECIMALS,
                actual: conf.decimals,
            });
        }

        validate_genesis_block_id(&conf.genesis_block_id)?;

        let api_urls = validate_api_urls(&request.client_conf.api_urls)?;
        let api_client = MintlayerApiClient::new(api_urls.clone());
        let node_conf = request.node_conf.as_ref().map(validate_node_rpc_config).transpose()?;

        let required_confirmations = request.required_confirmations.unwrap_or(conf.required_confirmations);

        if required_confirmations == 0 {
            return Err(MintlayerCoinBuildError::InvalidRequiredConfirmations);
        }

        let (key_pair, address, derivation_method) = build_mintlayer_identity(conf.network, priv_key_build_policy)?;
        let abortable_system = ctx
            .abortable_system
            .create_subsystem()
            .map_err(|error| MintlayerCoinBuildError::AbortableSystem(error.to_string()))?;

        Ok(MintlayerCoin(Arc::new(MintlayerCoinImpl {
            conf,
            api_urls,
            api_client,
            node_conf,
            tx_history: request.tx_history,
            required_confirmations: AtomicU64::new(required_confirmations),
            key_pair,
            address,
            derivation_method,
            abortable_system: Arc::new(abortable_system),
        })))
    }

    pub fn ticker(&self) -> &str {
        &self.conf.ticker
    }

    pub fn network(&self) -> MintlayerNetwork {
        self.conf.network
    }

    pub fn decimals(&self) -> u8 {
        self.conf.decimals
    }

    pub fn genesis_block_id(&self) -> &str {
        &self.conf.genesis_block_id
    }

    pub fn api_urls(&self) -> &[Url] {
        &self.api_urls
    }

    pub fn api_client(&self) -> &MintlayerApiClient {
        &self.api_client
    }

    pub fn node_rpc_url(&self) -> Option<&str> {
        self.node_conf.as_ref().map(|config| config.rpc_url.as_str())
    }

    pub fn node_client(&self) -> Result<Option<MintlayerNodeClient>, String> {
        let Some(config) = self.node_conf.as_ref() else {
            return Ok(None);
        };

        let client = if let Some(cookie_file) = config.rpc_cookie_file.as_deref() {
            let cookie = std::fs::read_to_string(cookie_file)
                .map_err(|error| format!("Failed to read Mintlayer node RPC cookie file: {error}"))?;
            let cookie = cookie.trim_end_matches(['\r', '\n']);
            let (username, password) = cookie
                .split_once(':')
                .ok_or_else(|| "Invalid Mintlayer node RPC cookie format".to_owned())?;
            if username.is_empty() || password.is_empty() {
                return Err("Invalid Mintlayer node RPC cookie format".to_owned());
            }
            MintlayerNodeClient::builder(config.rpc_url.clone())
                .basic_auth(username, password)
                .build()
                .map_err(|error| format!("Failed to build authenticated Mintlayer node RPC client: {error}"))?
        } else {
            MintlayerNodeClient::new(config.rpc_url.clone())
        };
        Ok(Some(client))
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn public_key(&self) -> &[u8] {
        self.key_pair.public_slice()
    }

    pub fn derivation_method(&self) -> &DerivationMethodResponse {
        &self.derivation_method
    }

    /// Verifies that the configured genesis belongs to the network served by the API.
    pub async fn validate_network(&self) -> Result<(), MintlayerNetworkValidationError> {
        let genesis = self.api_client.genesis().await?;
        validate_api_genesis_block_id(self.genesis_block_id(), &genesis.block_id)
    }

    pub async fn chain_tip(&self) -> Result<MintlayerChainTip, MintlayerApiError> {
        self.api_client.chain_tip().await
    }

    pub async fn address_info(&self, address: &str) -> Result<MintlayerAddressInfo, MintlayerApiError> {
        self.api_client.address_info(address).await
    }

    /// Returns the native coin balance of the locally derived Mintlayer address.
    pub async fn balance(&self) -> Result<CoinBalance, BalanceError> {
        let address_info = self
            .address_info(self.address())
            .await
            .map_err(|error| BalanceError::Transport(error.to_string()))?;

        coin_balance_from_address_info(address_info)
    }

    pub async fn spendable_utxos(&self, address: &str) -> Result<Vec<MintlayerUtxo>, MintlayerApiError> {
        self.api_client.spendable_utxos(address).await
    }

    pub fn tx_history_enabled(&self) -> bool {
        self.tx_history
    }

    pub fn required_confirmations(&self) -> u64 {
        self.required_confirmations.load(Ordering::Relaxed)
    }

    pub fn set_required_confirmations(&self, confirmations: u64) -> Result<(), MintlayerCoinBuildError> {
        if confirmations == 0 {
            return Err(MintlayerCoinBuildError::InvalidRequiredConfirmations);
        }

        self.required_confirmations.store(confirmations, Ordering::Relaxed);
        Ok(())
    }
}

fn mintlayer_sdk_network(network: MintlayerNetwork) -> SdkNetwork {
    match network {
        MintlayerNetwork::Mainnet => SdkNetwork::Mainnet,
        MintlayerNetwork::Testnet => SdkNetwork::Testnet,
        MintlayerNetwork::Regtest => SdkNetwork::Regtest,
        MintlayerNetwork::Signet => SdkNetwork::Signet,
    }
}

fn mintlayer_atoms_from_decimal(amount: &BigDecimal) -> Result<u128, String> {
    let rendered = amount.to_string();
    let rendered = rendered.strip_prefix('+').unwrap_or(&rendered);
    if rendered.starts_with('-') {
        return Err("Mintlayer withdrawal amount cannot be negative".into());
    }

    let mut parts = rendered.split('.');
    let integer = parts.next().unwrap_or_default();
    let fractional = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || integer.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fractional.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("Invalid Mintlayer decimal amount: {rendered}"));
    }

    let decimals = MINTLAYER_DECIMALS as usize;
    let (kept_fractional, excess_fractional) = if fractional.len() > decimals {
        fractional.split_at(decimals)
    } else {
        (fractional, "")
    };
    if !excess_fractional.bytes().all(|byte| byte == b'0') {
        return Err(format!(
            "Mintlayer amount has more than {MINTLAYER_DECIMALS} decimal places: {rendered}"
        ));
    }

    let scale = 10_u128.pow(MINTLAYER_DECIMALS as u32);
    let integer_atoms = integer
        .parse::<u128>()
        .map_err(|error| format!("Invalid Mintlayer amount '{rendered}': {error}"))?
        .checked_mul(scale)
        .ok_or_else(|| format!("Mintlayer amount overflow: {rendered}"))?;

    let mut padded_fractional = kept_fractional.to_owned();
    padded_fractional.push_str(&"0".repeat(decimals - kept_fractional.len()));
    let fractional_atoms = if padded_fractional.is_empty() {
        0
    } else {
        padded_fractional
            .parse::<u128>()
            .map_err(|error| format!("Invalid Mintlayer amount '{rendered}': {error}"))?
    };

    integer_atoms
        .checked_add(fractional_atoms)
        .ok_or_else(|| format!("Mintlayer amount overflow: {rendered}"))
}

fn mintlayer_decimal_from_atoms(atoms: u128) -> BigDecimal {
    BigDecimal::new(BigInt::from(atoms), MINTLAYER_DECIMALS as i64)
}

fn mintlayer_withdraw_details(
    ticker: &str,
    sender: String,
    recipient: String,
    plan: MintlayerSignedTransactionPlan,
) -> Result<TransactionDetails, WithdrawError> {
    let transaction_id = plan.transaction_id.clone();
    let internal_id = hex::decode(&transaction_id)
        .map_err(|error| WithdrawError::InternalError(format!("Invalid Mintlayer transaction ID: {error}")))?;
    let spent_by_me = mintlayer_decimal_from_atoms(plan.selected_atoms);
    let received_by_me = mintlayer_decimal_from_atoms(plan.change_atoms);
    let fee_amount = mintlayer_decimal_from_atoms(plan.fee_atoms);

    Ok(TransactionDetails {
        tx: TransactionData::new_signed(plan.signed_bytes.into(), transaction_id),
        from: vec![sender],
        to: vec![recipient],
        total_amount: spent_by_me.clone(),
        spent_by_me: spent_by_me.clone(),
        received_by_me: received_by_me.clone(),
        my_balance_change: received_by_me - spent_by_me,
        block_height: 0,
        timestamp: now_sec(),
        fee_details: Some(TxFeeDetails::Utxo(UtxoFeeDetails {
            coin: Some(ticker.to_owned()),
            amount: fee_amount,
        })),
        coin: ticker.to_owned(),
        internal_id: internal_id.into(),
        kmd_rewards: None,
        transaction_type: Default::default(),
        memo: None,
    })
}

async fn broadcast_mintlayer_transaction_data(coin: &MintlayerCoin, tx: &TransactionData) -> Result<String, String> {
    let node_client = coin
        .node_client()?
        .ok_or_else(|| "Mintlayer node RPC is not configured".to_owned())?;
    let tx_hex = tx
        .tx_hex()
        .ok_or_else(|| "Mintlayer broadcast requires signed transaction data".to_owned())?;
    let tx_hash = tx
        .tx_hash()
        .ok_or_else(|| "Mintlayer broadcast requires a canonical transaction ID".to_owned())?
        .to_owned();

    broadcast_signed_transaction_hex(&node_client, &hex::encode(&tx_hex.0))
        .await
        .map_err(|error| error.to_string())?;

    Ok(tx_hash)
}

async fn build_mintlayer_withdraw(
    coin: MintlayerCoin,
    req: WithdrawRequest,
) -> Result<TransactionDetails, WithdrawError> {
    if req.coin != coin.ticker() {
        return Err(WithdrawError::UnsupportedError(format!(
            "Mintlayer withdraw request coin '{}' does not match '{}'",
            req.coin,
            coin.ticker()
        )));
    }
    if req.from.is_some() {
        return Err(WithdrawError::UnsupportedError(
            "Mintlayer withdraw does not support an alternate from address yet".into(),
        ));
    }
    if req.max {
        return Err(WithdrawError::UnsupportedError(
            "Mintlayer withdraw max mode is not implemented yet".into(),
        ));
    }
    if req.fee.is_some() {
        return Err(WithdrawError::InvalidFeePolicy(
            "Mintlayer withdraw currently uses the canonical API fee rate only".into(),
        ));
    }
    if req.memo.is_some() {
        return Err(WithdrawError::UnsupportedError(
            "Mintlayer withdraw memo is not supported".into(),
        ));
    }
    if req.ibc_source_channel.is_some() || req.expiration_seconds.is_some() {
        return Err(WithdrawError::UnsupportedError(
            "Unsupported protocol-specific Mintlayer withdraw fields".into(),
        ));
    }
    validate_mintlayer_address(coin.network(), &req.to)
        .map_err(|error| WithdrawError::InvalidAddress(error.to_string()))?;
    let send_atoms = mintlayer_atoms_from_decimal(&req.amount).map_err(WithdrawError::InternalError)?;
    if send_atoms == 0 {
        return Err(WithdrawError::UnsupportedError(
            "Mintlayer withdrawal amount must be greater than zero".into(),
        ));
    }

    let sender = coin.address().to_owned();
    let utxos = coin
        .spendable_utxos(&sender)
        .await
        .map_err(|error| WithdrawError::Transport(error.to_string()))?;
    let fee_rate = coin
        .api_client()
        .fee_rate()
        .await
        .map_err(|error| WithdrawError::Transport(error.to_string()))?;
    let chain_tip = coin
        .chain_tip()
        .await
        .map_err(|error| WithdrawError::Transport(error.to_string()))?;

    let sdk_private_key = sdk_private_key_from_kdf_key_pair(&coin.key_pair)
        .map_err(|error| WithdrawError::InternalError(error.to_string()))?;
    let plan = plan_signed_transaction_offline(
        &utxos,
        &sender,
        &req.to,
        send_atoms,
        fee_rate,
        &sdk_private_key,
        chain_tip.block_height,
        mintlayer_sdk_network(coin.network()),
    )
    .map_err(|error| WithdrawError::InternalError(error.to_string()))?;
    drop(sdk_private_key);

    mintlayer_withdraw_details(coin.ticker(), sender, req.to, plan)
}

async fn build_mintlayer_swap_payment(
    coin: &MintlayerCoin,
    args: &SendPaymentArgs<'_>,
) -> Result<MintlayerTransaction, String> {
    let send_atoms = mintlayer_atoms_from_decimal(&args.amount)?;
    if send_atoms == 0 {
        return Err("Mintlayer swap payment amount must be greater than zero".into());
    }

    coin.validate_other_pubkey(args.other_pubkey)
        .map_err(|error| error.to_string())?;

    let spend_address = mintlayer_address_from_compressed_public_key(coin.network(), args.other_pubkey)
        .map_err(|error| error.to_string())?;

    let htlc_key_pair = coin.derive_htlc_key_pair(args.swap_unique_data);
    let refund_address = mintlayer_address_from_compressed_public_key(coin.network(), htlc_key_pair.public_slice())
        .map_err(|error| error.to_string())?;

    let network = mintlayer_sdk_network(coin.network());

    let payment_output = build_mintlayer_htlc_output(
        send_atoms,
        args.secret_hash,
        &spend_address,
        &refund_address,
        args.time_lock,
        network,
    )
    .map_err(|error| error.to_string())?;

    let sender = coin.address().to_owned();
    let utxos = coin.spendable_utxos(&sender).await.map_err(|error| error.to_string())?;

    let fee_rate = coin.api_client().fee_rate().await.map_err(|error| error.to_string())?;

    let chain_tip = coin.chain_tip().await.map_err(|error| error.to_string())?;

    let sdk_private_key = sdk_private_key_from_kdf_key_pair(&coin.key_pair).map_err(|error| error.to_string())?;

    let plan = plan_signed_output_offline(
        &utxos,
        &sender,
        payment_output,
        send_atoms,
        fee_rate,
        &sdk_private_key,
        chain_tip.block_height,
        network,
    )
    .map_err(|error| error.to_string())?;

    drop(sdk_private_key);

    Ok(MintlayerTransaction {
        signed_bytes: plan.signed_bytes,
        transaction_id: plan.transaction_id,
    })
}

async fn send_mintlayer_swap_payment(coin: &MintlayerCoin, args: &SendPaymentArgs<'_>) -> TransactionResult {
    let transaction = build_mintlayer_swap_payment(coin, args)
        .await
        .map_err(TransactionErr::Plain)?;

    let broadcast_txid = coin
        .send_raw_tx_bytes(&transaction.signed_bytes)
        .compat()
        .await
        .map_err(TransactionErr::Plain)?;

    if broadcast_txid != transaction.transaction_id {
        return Err(TransactionErr::Plain(format!(
            "Mintlayer broadcaster returned transaction ID '{}' but built transaction ID is '{}'",
            broadcast_txid, transaction.transaction_id
        )));
    }

    Ok(transaction.into())
}

#[async_trait]
impl MarketCoinOps for MintlayerCoin {
    fn ticker(&self) -> &str {
        &self.conf.ticker
    }

    fn my_address(&self) -> MmResult<String, MyAddressError> {
        Ok(self.address.clone())
    }

    fn address_from_pubkey(&self, pubkey: &H264Json) -> MmResult<String, AddressFromPubkeyError> {
        mintlayer_address_from_compressed_public_key(self.network(), &pubkey.0)
            .map_err(|error| AddressFromPubkeyError::InternalError(error.to_string()).into())
    }

    async fn get_public_key(&self) -> Result<String, MmError<UnexpectedDerivationMethod>> {
        Ok(hex::encode(self.public_key()))
    }

    fn sign_message_hash(&self, _message: &str) -> Option<[u8; 32]> {
        None
    }

    fn sign_message(&self, _message: &str, _address: Option<HDAddressSelector>) -> SignatureResult<String> {
        MmError::err(SignatureError::InternalError(
            "Mintlayer message signing is not implemented".to_string(),
        ))
    }

    fn verify_message(&self, _signature: &str, _message: &str, _address: &str) -> VerificationResult<bool> {
        MmError::err(VerificationError::InternalError(
            "Mintlayer message verification is not implemented".to_string(),
        ))
    }

    fn my_balance(&self) -> BalanceFut<CoinBalance> {
        let coin = self.clone();
        let future = async move {
            match coin.balance().await {
                Ok(balance) => Ok(balance),
                Err(error) => MmError::err(error),
            }
        };
        Box::new(future.boxed().compat())
    }

    fn platform_coin_balance(&self) -> BalanceFut<BigDecimal> {
        Box::new(self.my_balance().map(|balance| balance.spendable))
    }

    fn platform_ticker(&self) -> &str {
        self.ticker()
    }

    fn send_raw_tx(&self, tx: &str) -> Box<dyn Future<Item = String, Error = String> + Send> {
        let bytes = match hex::decode(tx) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Box::new(futures01::future::err(format!(
                    "Invalid Mintlayer transaction hex: {error}"
                )));
            },
        };
        self.send_raw_tx_bytes(&bytes)
    }

    fn send_raw_tx_bytes(&self, tx: &[u8]) -> Box<dyn Future<Item = String, Error = String> + Send> {
        let tx_id = match canonical_transaction_id_from_signed_bytes(tx) {
            Ok(tx_id) => tx_id,
            Err(error) => return Box::new(futures01::future::err(error.to_string())),
        };
        let node_client = match self.node_client() {
            Ok(Some(client)) => client,
            Ok(None) => {
                return Box::new(futures01::future::err(
                    "Mintlayer node RPC is not configured".to_string(),
                ));
            },
            Err(error) => return Box::new(futures01::future::err(error)),
        };
        let tx_hex = hex::encode(tx);
        let future = async move {
            broadcast_signed_transaction_hex(&node_client, &tx_hex)
                .await
                .map_err(|error| error.to_string())?;
            Ok(tx_id)
        };
        Box::new(future.boxed().compat())
    }

    fn wait_for_confirmations(&self, _input: ConfirmPaymentInput) -> Box<dyn Future<Item = (), Error = String> + Send> {
        Box::new(futures01::future::err(
            "Mintlayer confirmation tracking is not implemented".to_string(),
        ))
    }

    async fn wait_for_htlc_tx_spend(&self, _args: WaitForHTLCTxSpendArgs<'_>) -> TransactionResult {
        Err(TransactionErr::ProtocolNotSupported(
            "Mintlayer HTLC transaction tracking is not implemented".to_string(),
        ))
    }

    fn tx_enum_from_bytes(&self, _bytes: &[u8]) -> Result<TransactionEnum, MmError<TxMarshalingErr>> {
        MmError::err(TxMarshalingErr::NotSupported(
            "Mintlayer transaction decoding is not implemented".to_string(),
        ))
    }

    fn current_block(&self) -> Box<dyn Future<Item = u64, Error = String> + Send> {
        let coin = self.clone();
        let future = async move {
            coin.chain_tip()
                .await
                .map(|tip| tip.block_height)
                .map_err(|error| error.to_string())
        };
        Box::new(future.boxed().compat())
    }

    fn display_priv_key(&self) -> Result<String, String> {
        Err("Mintlayer private-key export is disabled".to_string())
    }

    fn min_tx_amount(&self) -> BigDecimal {
        BigDecimal::new(BigInt::from(1), i64::from(MINTLAYER_DECIMALS))
    }

    fn min_trading_vol(&self) -> MmNumber {
        self.min_tx_amount().into()
    }

    fn should_burn_dex_fee(&self) -> bool {
        false
    }

    fn is_trezor(&self) -> bool {
        false
    }
}

const MINTLAYER_WALLET_ONLY_REASON: &str = "Mintlayer is currently wallet-only; swap transactions are not implemented";

#[async_trait]
impl SwapOps for MintlayerCoin {
    async fn send_taker_fee(&self, _dex_fee: DexFee, _uuid: &[u8], _expire_at: u64) -> TransactionResult {
        Err(TransactionErr::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    async fn send_maker_payment(&self, args: SendPaymentArgs<'_>) -> TransactionResult {
        send_mintlayer_swap_payment(self, &args).await
    }

    async fn send_taker_payment(&self, args: SendPaymentArgs<'_>) -> TransactionResult {
        send_mintlayer_swap_payment(self, &args).await
    }

    async fn send_maker_spends_taker_payment(&self, _args: SpendPaymentArgs<'_>) -> TransactionResult {
        Err(TransactionErr::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    async fn send_taker_spends_maker_payment(&self, _args: SpendPaymentArgs<'_>) -> TransactionResult {
        Err(TransactionErr::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    async fn send_taker_refunds_payment(&self, _args: RefundPaymentArgs<'_>) -> TransactionResult {
        Err(TransactionErr::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    async fn send_maker_refunds_payment(&self, _args: RefundPaymentArgs<'_>) -> TransactionResult {
        Err(TransactionErr::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    async fn validate_fee(&self, _args: ValidateFeeArgs<'_>) -> ValidatePaymentResult<()> {
        MmError::err(ValidatePaymentError::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    async fn validate_maker_payment(&self, _input: ValidatePaymentInput) -> ValidatePaymentResult<()> {
        MmError::err(ValidatePaymentError::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    async fn validate_taker_payment(&self, _input: ValidatePaymentInput) -> ValidatePaymentResult<()> {
        MmError::err(ValidatePaymentError::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    async fn check_if_my_payment_sent(
        &self,
        _args: CheckIfMyPaymentSentArgs<'_>,
    ) -> Result<Option<TransactionEnum>, String> {
        Err(MINTLAYER_WALLET_ONLY_REASON.into())
    }

    async fn search_for_swap_tx_spend_my(
        &self,
        _input: SearchForSwapTxSpendInput<'_>,
    ) -> Result<Option<FoundSwapTxSpend>, String> {
        Err(MINTLAYER_WALLET_ONLY_REASON.into())
    }

    async fn search_for_swap_tx_spend_other(
        &self,
        _input: SearchForSwapTxSpendInput<'_>,
    ) -> Result<Option<FoundSwapTxSpend>, String> {
        Err(MINTLAYER_WALLET_ONLY_REASON.into())
    }

    async fn extract_secret(&self, _secret_hash: &[u8], _spend_tx: &[u8]) -> Result<[u8; 32], String> {
        Err(MINTLAYER_WALLET_ONLY_REASON.into())
    }

    fn negotiate_swap_contract_addr(
        &self,
        _other_side_address: Option<&[u8]>,
    ) -> Result<Option<BytesJson>, MmError<NegotiateSwapContractAddrErr>> {
        Ok(None)
    }

    fn derive_htlc_key_pair(&self, _swap_unique_data: &[u8]) -> KeyPair {
        self.key_pair.clone()
    }

    fn derive_htlc_pubkey(&self, _swap_unique_data: &[u8]) -> [u8; 33] {
        let mut public_key = [0_u8; 33];
        public_key.copy_from_slice(self.public_key());
        public_key
    }

    fn validate_other_pubkey(&self, raw_pubkey: &[u8]) -> MmResult<(), ValidateOtherPubKeyErr> {
        secp256k1::PublicKey::from_slice(raw_pubkey)
            .map(|_| ())
            .map_err(|error| ValidateOtherPubKeyErr::InvalidPubKey(error.to_string()).into())
    }
}

impl WatcherOps for MintlayerCoin {}

#[async_trait]
impl MmCoin for MintlayerCoin {
    fn wallet_only(&self, _ctx: &MmArc) -> bool {
        true
    }

    fn spawner(&self) -> WeakSpawner {
        self.abortable_system.weak_spawner()
    }

    fn withdraw(&self, req: WithdrawRequest) -> WithdrawFut {
        let coin = self.clone();
        let future = async move {
            let broadcast = req.broadcast;
            let details = build_mintlayer_withdraw(coin.clone(), req)
                .await
                .map_err(MmError::new)?;

            if broadcast {
                broadcast_mintlayer_transaction_data(&coin, &details.tx)
                    .await
                    .map_err(|error| MmError::new(WithdrawError::Transport(error)))?;
            }

            Ok(details)
        };
        Box::new(future.boxed().compat())
    }

    fn get_raw_transaction(&self, _req: RawTransactionRequest) -> RawTransactionFut<'_> {
        Box::new(futures01::future::err(MmError::new(
            RawTransactionError::NotImplemented {
                coin: self.ticker().to_string(),
            },
        )))
    }

    fn get_tx_hex_by_hash(&self, _tx_hash: Vec<u8>) -> RawTransactionFut<'_> {
        Box::new(futures01::future::err(MmError::new(
            RawTransactionError::NotImplemented {
                coin: self.ticker().to_string(),
            },
        )))
    }

    fn decimals(&self) -> u8 {
        self.decimals()
    }

    fn convert_to_address(&self, _from: &str, _to_address_format: Json) -> Result<String, String> {
        Err("Mintlayer address conversion is not implemented".into())
    }

    fn validate_address(&self, address: &str) -> ValidateAddressResult {
        match validate_mintlayer_address(self.network(), address) {
            Ok(()) => ValidateAddressResult {
                is_valid: true,
                reason: None,
            },
            Err(error) => ValidateAddressResult {
                is_valid: false,
                reason: Some(error.to_string()),
            },
        }
    }

    fn process_history_loop(&self, _ctx: MmArc) -> Box<dyn Future<Item = (), Error = ()> + Send> {
        Box::new(futures01::future::ok(()))
    }

    fn history_sync_status(&self) -> HistorySyncState {
        HistorySyncState::NotEnabled
    }

    fn get_trade_fee(&self) -> Box<dyn Future<Item = TradeFee, Error = String> + Send> {
        Box::new(futures01::future::err(MINTLAYER_WALLET_ONLY_REASON.into()))
    }

    async fn get_sender_trade_fee(
        &self,
        _value: TradePreimageValue,
        _stage: FeeApproxStage,
    ) -> TradePreimageResult<TradeFee> {
        MmError::err(TradePreimageError::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    fn get_receiver_trade_fee(&self, _stage: FeeApproxStage) -> TradePreimageFut<TradeFee> {
        Box::new(futures01::future::err(MmError::new(
            TradePreimageError::ProtocolNotSupported(MINTLAYER_WALLET_ONLY_REASON.into()),
        )))
    }

    async fn get_fee_to_send_taker_fee(
        &self,
        _dex_fee_amount: DexFee,
        _stage: FeeApproxStage,
    ) -> TradePreimageResult<TradeFee> {
        MmError::err(TradePreimageError::ProtocolNotSupported(
            MINTLAYER_WALLET_ONLY_REASON.into(),
        ))
    }

    fn required_confirmations(&self) -> u64 {
        self.required_confirmations()
    }

    fn requires_notarization(&self) -> bool {
        false
    }

    fn set_required_confirmations(&self, confirmations: u64) {
        if confirmations > 0 {
            self.required_confirmations.store(confirmations, Ordering::Relaxed);
        }
    }

    fn set_requires_notarization(&self, _requires_nota: bool) {}

    fn swap_contract_address(&self) -> Option<BytesJson> {
        None
    }

    fn fallback_swap_contract(&self) -> Option<BytesJson> {
        None
    }

    fn mature_confirmations(&self) -> Option<u32> {
        None
    }

    fn coin_protocol_info(&self, _amount_to_receive: Option<MmNumber>) -> Vec<u8> {
        Vec::new()
    }

    fn is_coin_protocol_supported(
        &self,
        _info: &Option<Vec<u8>>,
        _amount_to_send: Option<MmNumber>,
        _locktime: u64,
        _is_maker: bool,
    ) -> bool {
        false
    }

    fn on_disabled(&self) -> Result<(), AbortedError> {
        self.abortable_system.abort_all()
    }

    fn on_token_deactivated(&self, _ticker: &str) {}
}

fn coin_balance_from_address_info(address_info: MintlayerAddressInfo) -> Result<CoinBalance, BalanceError> {
    let spendable = address_info
        .coin_balance
        .to_big_decimal()
        .map_err(|error| BalanceError::InvalidResponse(error.to_string()))?;
    let unspendable = address_info
        .locked_coin_balance
        .to_big_decimal()
        .map_err(|error| BalanceError::InvalidResponse(error.to_string()))?;

    Ok(CoinBalance { spendable, unspendable })
}

fn build_mintlayer_identity(
    network: MintlayerNetwork,
    priv_key_build_policy: PrivKeyBuildPolicy,
) -> Result<(KeyPair, String, DerivationMethodResponse), MintlayerCoinBuildError> {
    let (secret, derivation_method) = match priv_key_build_policy {
        PrivKeyBuildPolicy::IguanaPrivKey(secret) => (secret, DerivationMethodResponse::Iguana),
        PrivKeyBuildPolicy::GlobalHDAccount(global_hd_account) => {
            let derivation_path = mintlayer_derivation_path(network, 0, Bip44Chain::External, 0)
                .map_err(|error| MintlayerCoinBuildError::KeyDerivation(error.to_string()))?;
            let secret = global_hd_account
                .derive_secp256k1_secret(&derivation_path)
                .map_err(|error| MintlayerCoinBuildError::KeyDerivation(error.to_string()))?;

            (secret, DerivationMethodResponse::HDWallet(derivation_path.to_string()))
        },
        PrivKeyBuildPolicy::Trezor => {
            return Err(MintlayerCoinBuildError::UnsupportedPrivKeyPolicy { policy: "Trezor" })
        },
        PrivKeyBuildPolicy::WalletConnect { .. } => {
            return Err(MintlayerCoinBuildError::UnsupportedPrivKeyPolicy {
                policy: "WalletConnect",
            })
        },
    };

    let key_pair = key_pair_from_secret(&secret.take())
        .map_err(|error| MintlayerCoinBuildError::KeyDerivation(error.to_string()))?;
    let address = mintlayer_address_from_compressed_public_key(network, key_pair.public_slice())
        .map_err(|error| MintlayerCoinBuildError::AddressDerivation(error.to_string()))?;

    Ok((key_pair, address, derivation_method))
}

fn is_valid_genesis_block_id(genesis_block_id: &str) -> bool {
    genesis_block_id.len() == 64
        && hex::decode(genesis_block_id)
            .map(|bytes| bytes.len() == 32)
            .unwrap_or(false)
}

fn validate_genesis_block_id(genesis_block_id: &str) -> Result<(), MintlayerCoinBuildError> {
    if !is_valid_genesis_block_id(genesis_block_id) {
        return Err(MintlayerCoinBuildError::InvalidGenesisBlockId);
    }

    Ok(())
}

fn validate_api_genesis_block_id(expected: &str, actual: &str) -> Result<(), MintlayerNetworkValidationError> {
    if !is_valid_genesis_block_id(actual) {
        return Err(MintlayerNetworkValidationError::InvalidGenesisBlockId {
            actual: actual.to_owned(),
        });
    }

    if !expected.eq_ignore_ascii_case(actual) {
        return Err(MintlayerNetworkValidationError::UnexpectedGenesisBlockId {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        });
    }

    Ok(())
}

fn validate_node_rpc_config(
    config: &MintlayerNodeClientConfig,
) -> Result<MintlayerNodeClientConfig, MintlayerCoinBuildError> {
    let url = Url::parse(&config.rpc_url).map_err(|_| MintlayerCoinBuildError::InvalidNodeRpcUrl)?;

    if url.username() != "" || url.password().is_some() {
        return Err(MintlayerCoinBuildError::NodeRpcUrlCredentialsNotAllowed);
    }

    if url.scheme() != "http" {
        return Err(MintlayerCoinBuildError::UnsupportedNodeRpcUrlScheme);
    }

    let is_loopback = match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false),
        None => false,
    };
    if !is_loopback {
        return Err(MintlayerCoinBuildError::RemoteNodeRpcUrlNotAllowed);
    }

    Ok(MintlayerNodeClientConfig {
        rpc_url: url.to_string().trim_end_matches('/').to_owned(),
        rpc_cookie_file: config.rpc_cookie_file.clone(),
    })
}

fn validate_api_urls(configured_urls: &[String]) -> Result<Vec<Url>, MintlayerCoinBuildError> {
    if configured_urls.is_empty() {
        return Err(MintlayerCoinBuildError::MissingApiUrls);
    }

    let mut normalized_urls = Vec::with_capacity(configured_urls.len());
    let mut unique_urls = HashSet::with_capacity(configured_urls.len());

    for (index, configured_url) in configured_urls.iter().enumerate() {
        let mut url = Url::parse(configured_url).map_err(|_| MintlayerCoinBuildError::InvalidApiUrl { index })?;

        if url.username() != "" || url.password().is_some() {
            return Err(MintlayerCoinBuildError::ApiUrlCredentialsNotAllowed { index });
        }

        if url.query().is_some() || url.fragment().is_some() {
            return Err(MintlayerCoinBuildError::InvalidApiUrl { index });
        }

        match url.scheme() {
            "https" => {},
            "http" if is_loopback(&url) => {},
            "http" => return Err(MintlayerCoinBuildError::InsecureRemoteApiUrl { index }),
            _ => return Err(MintlayerCoinBuildError::UnsupportedApiUrlScheme { index }),
        }

        if url.path() == "/" {
            url.set_path("");
        }

        let normalized = url.as_str().to_owned();
        if !unique_urls.insert(normalized) {
            return Err(MintlayerCoinBuildError::DuplicateApiUrl { index });
        }

        normalized_urls.push(url);
    }

    Ok(normalized_urls)
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mintlayer::MintlayerApiClientConfig;
    use crypto::{CryptoCtx, KeyPairPolicy};
    use futures::compat::Future01CompatExt;
    use futures01::Future as Future01;
    use mm2_core::mm_ctx::{MmArc, MmCtxBuilder};
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::{Duration, Instant};

    const WITHDRAW_RECIPIENT: &str = "mtc1qxlpcx3rzm4nlqw2a2atsw9gtuv6lvaeasdrqxjz";
    const WITHDRAW_FEE_RATE_ATOMS_PER_KB: u128 = 100_000_000_000;

    fn decimal(value: &str) -> BigDecimal {
        value.parse().unwrap()
    }

    fn withdraw_request(to: &str) -> WithdrawRequest {
        WithdrawRequest {
            coin: "ML".into(),
            from: None,
            to: to.into(),
            amount: decimal("0.5"),
            max: false,
            fee: None,
            memo: None,
            ibc_source_channel: None,
            broadcast: false,
            expiration_seconds: None,
        }
    }

    fn bind_withdraw_mock_api() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let api_url = format!("http://{}", listener.local_addr().unwrap());
        (listener, api_url)
    }

    fn read_request_path(stream: &mut TcpStream) -> String {
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];

        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            assert!(request.len() <= 16 * 1024, "mock HTTP request too large");
        }

        let request = String::from_utf8(request).unwrap();
        request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("HTTP request path")
            .to_owned()
    }

    fn respond_json(stream: &mut TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.as_bytes().len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    fn spawn_withdraw_mock_api(listener: TcpListener, sender: String) -> thread::JoinHandle<Vec<String>> {
        thread::spawn(move || {
            let spendable_path = format!("/api/v2/address/{sender}/spendable-utxos");
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut requested_paths = Vec::new();

            while requested_paths.len() < 3 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let path = read_request_path(&mut stream);
                        let body = match path.as_str() {
                            value if value == spendable_path => json!([{
                                "outpoint": {
                                    "index": 0,
                                    "source_id": "11".repeat(32),
                                    "source_type": "Transaction"
                                },
                                "utxo": {
                                    "destination": sender.clone(),
                                    "type": "Transfer",
                                    "value": {
                                        "amount": { "atoms": "100000000000", "decimal": "1" },
                                        "type": "Coin"
                                    }
                                }
                            }])
                            .to_string(),
                            "/api/v2/feerate" => format!("\"{WITHDRAW_FEE_RATE_ATOMS_PER_KB}\""),
                            "/api/v2/chain/tip" => json!({
                                "block_height": 700000,
                                "block_id": "22".repeat(32)
                            })
                            .to_string(),
                            other => panic!("unexpected mock API request: {}", other),
                        };

                        respond_json(&mut stream, &body);
                        requested_paths.push(path);
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    },
                    Err(error) => panic!("mock API accept failed: {}", error),
                }
            }

            assert_eq!(requested_paths.len(), 3, "mock API did not receive all requests");
            requested_paths
        })
    }

    #[test]
    fn mmcoin_withdraw_builds_signed_transaction_from_loopback_api() {
        let (listener, api_url) = bind_withdraw_mock_api();
        let coin = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec![api_url]),
            iguana_policy(),
        )
        .unwrap();
        let sender = coin.address().to_owned();
        let server = spawn_withdraw_mock_api(listener, sender.clone());

        let details = MmCoin::withdraw(&coin, withdraw_request(WITHDRAW_RECIPIENT))
            .wait()
            .unwrap();
        let requested_paths = server.join().unwrap();

        assert_eq!(
            requested_paths,
            vec![
                format!("/api/v2/address/{sender}/spendable-utxos"),
                "/api/v2/feerate".to_owned(),
                "/api/v2/chain/tip".to_owned(),
            ]
        );
        assert_eq!(details.from, vec![sender]);
        assert_eq!(details.to, vec![WITHDRAW_RECIPIENT.to_owned()]);
        assert_eq!(details.total_amount, decimal("1"));
        assert_eq!(details.spent_by_me, decimal("1"));
        assert_eq!(details.received_by_me, decimal("0.296"));
        assert_eq!(details.my_balance_change, decimal("-0.704"));
        assert_eq!(details.block_height, 0);
        assert!(details.timestamp > 0);
        assert_eq!(details.coin, "ML");
        assert_eq!(details.internal_id.0.len(), 32);

        match &details.tx {
            TransactionData::Signed { tx_hex, tx_hash } => {
                assert_eq!(tx_hex.0.len(), 204);
                assert_eq!(hex::decode(tx_hash).unwrap().len(), 32);
            },
            other => panic!("expected signed Mintlayer transaction, got {:?}", other),
        }

        match details.fee_details.as_ref() {
            Some(TxFeeDetails::Utxo(fee)) => {
                assert_eq!(fee.coin.as_deref(), Some("ML"));
                assert_eq!(fee.amount, decimal("0.204"));
            },
            other => panic!("expected Mintlayer UTXO fee details, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn mmcoin_withdraw_broadcasts_exact_signed_transaction_when_requested() {
        use std::sync::mpsc;

        let (api_listener, api_url) = bind_withdraw_mock_api();

        let node_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let node_endpoint = format!("http://{}", node_listener.local_addr().unwrap());
        let (tx_sender, tx_receiver) = mpsc::channel();

        let node_server = thread::spawn(move || {
            let (mut stream, _) = node_listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];

            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);

                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header_end = pos + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);

                    if request.len() >= header_end + content_length {
                        break;
                    }
                }

                assert!(request.len() <= 64 * 1024, "mock Node RPC request too large");
            }

            let body_start = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .expect("Node RPC HTTP body")
                + 4;

            let body: serde_json::Value = serde_json::from_slice(&request[body_start..]).unwrap();

            assert_eq!(body["method"], "p2p_submit_transaction");
            assert_eq!(body["params"]["options"]["trust_policy"], "Trusted");

            let submitted_hex = body["params"]["tx"]
                .as_str()
                .expect("params.tx must be a string")
                .to_owned();

            assert!(!submitted_hex.is_empty());
            hex::decode(&submitted_hex).expect("params.tx must contain valid hex");

            tx_sender.send(submitted_hex).unwrap();

            let response_body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": body["id"].clone(),
                "result": null
            })
            .to_string();

            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();

            stream.flush().unwrap();
        });

        let mut activation = request_with_urls(vec![api_url]);
        activation.node_conf = Some(MintlayerNodeClientConfig {
            rpc_url: node_endpoint,
            rpc_cookie_file: None,
        });

        let coin = MintlayerCoin::new(&test_ctx(), valid_conf(), activation, iguana_policy()).unwrap();

        let sender = coin.address().to_owned();
        let api_server = spawn_withdraw_mock_api(api_listener, sender);

        let mut request = withdraw_request(WITHDRAW_RECIPIENT);
        request.broadcast = true;

        let details = MmCoin::withdraw(&coin, request).compat().await.unwrap();

        api_server.join().unwrap();
        node_server.join().unwrap();

        let submitted_hex = tx_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("Node RPC did not receive the signed transaction");

        let (returned_bytes, returned_txid) = match &details.tx {
            TransactionData::Signed { tx_hex, tx_hash } => (tx_hex.0.clone(), tx_hash.clone()),
            other => panic!("expected signed Mintlayer transaction, got {:?}", other),
        };

        assert_eq!(submitted_hex, hex::encode(&returned_bytes));

        assert_eq!(
            canonical_transaction_id_from_signed_bytes(&returned_bytes).unwrap(),
            returned_txid
        );

        assert_eq!(details.to, vec![WITHDRAW_RECIPIENT.to_owned()]);

        match details.fee_details.as_ref() {
            Some(TxFeeDetails::Utxo(fee)) => {
                assert_eq!(fee.coin.as_deref(), Some("ML"));
            },
            other => panic!("expected Mintlayer UTXO fee details, got {:?}", other),
        }
    }

    #[test]
    fn mmcoin_withdraw_rejects_unsupported_requests_before_network() {
        let coin = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec!["http://127.0.0.1:9/api/v2".into()]),
            iguana_policy(),
        )
        .unwrap();

        let mut max = withdraw_request(WITHDRAW_RECIPIENT);
        max.max = true;
        let max_error = MmCoin::withdraw(&coin, max).wait().unwrap_err();
        assert!(format!("{max_error:?}").contains("max mode is not implemented"));

        let invalid_address = withdraw_request("not-a-mintlayer-address");
        let address_error = MmCoin::withdraw(&coin, invalid_address).wait().unwrap_err();
        assert!(format!("{address_error:?}").contains("InvalidAddress"));
    }

    const TEST_MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn valid_conf() -> MintlayerCoinConf {
        MintlayerCoinConf {
            ticker: "ML".into(),
            network: MintlayerNetwork::Mainnet,
            decimals: MINTLAYER_DECIMALS,
            required_confirmations: 2,
            genesis_block_id: "a".repeat(64),
        }
    }

    fn request_with_urls(api_urls: Vec<String>) -> MintlayerActivationRequest {
        MintlayerActivationRequest {
            tx_history: false,
            required_confirmations: None,
            client_conf: MintlayerApiClientConfig { api_urls },
            node_conf: None,
        }
    }

    fn iguana_policy() -> PrivKeyBuildPolicy {
        PrivKeyBuildPolicy::IguanaPrivKey([1_u8; 32].into())
    }

    fn test_ctx() -> MmArc {
        MmCtxBuilder::default().into_mm_arc()
    }

    #[tokio::test]
    async fn swap_payment_builder_produces_native_htlc_without_broadcast() {
        use mintlayer_sdk::crypto::types::TxOutput;

        const SECRET_HASH: [u8; 20] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x10, 0x20,
            0x30, 0x40,
        ];
        const TIME_LOCK: u64 = 1_800_000_000;

        let (listener, api_url) = bind_withdraw_mock_api();
        let coin = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec![api_url]),
            iguana_policy(),
        )
        .unwrap();

        let sender = coin.address().to_owned();
        let api_server = spawn_withdraw_mock_api(listener, sender.clone());

        let other_secret = [2_u8; 32];
        let other_key_pair = key_pair_from_secret(&other_secret.into()).unwrap();
        let other_pubkey = other_key_pair.public_slice().to_vec();

        let swap_contract_address = None;
        let payment_instructions = None;
        let amount: BigDecimal = "0.5".parse().unwrap();
        let swap_unique_data = b"mintlayer-swap-payment-builder-test";

        let args = SendPaymentArgs {
            time_lock_duration: 3600,
            time_lock: TIME_LOCK,
            other_pubkey: &other_pubkey,
            secret_hash: &SECRET_HASH,
            amount,
            swap_contract_address: &swap_contract_address,
            swap_unique_data,
            payment_instructions: &payment_instructions,
            watcher_reward: None,
            wait_for_confirmation_until: 0,
        };

        let transaction = build_mintlayer_swap_payment(&coin, &args).await.unwrap();

        api_server.join().unwrap();

        assert_eq!(
            canonical_transaction_id_from_signed_bytes(&transaction.signed_bytes).unwrap(),
            transaction.transaction_id
        );

        let decoded = mintlayer_sdk::crypto::decode_transaction_lenient(&transaction.signed_bytes).unwrap();

        assert!(
            matches!(decoded.outputs().first(), Some(TxOutput::Htlc(_, _))),
            "first swap-payment output must be a native Mintlayer HTLC"
        );

        let expected_spend_address =
            mintlayer_address_from_compressed_public_key(coin.network(), &other_pubkey).unwrap();

        let local_htlc_key = coin.derive_htlc_key_pair(swap_unique_data);
        let expected_refund_address =
            mintlayer_address_from_compressed_public_key(coin.network(), local_htlc_key.public_slice()).unwrap();

        let expected_output = build_mintlayer_htlc_output(
            50_000_000_000,
            &SECRET_HASH,
            &expected_spend_address,
            &expected_refund_address,
            TIME_LOCK,
            mintlayer_sdk_network(coin.network()),
        )
        .unwrap();

        assert_eq!(decoded.outputs().first(), Some(&expected_output));
    }

    #[tokio::test]
    async fn maker_payment_swapops_builds_broadcasts_and_returns_native_htlc() {
        use mintlayer_sdk::crypto::types::TxOutput;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        const SECRET_HASH: [u8; 20] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x10, 0x20,
            0x30, 0x40,
        ];
        const TIME_LOCK: u64 = 1_800_000_000;

        let (api_listener, api_url) = bind_withdraw_mock_api();

        let rpc_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let rpc_endpoint = format!("http://{}", rpc_listener.local_addr().unwrap());

        let mut request = request_with_urls(vec![api_url]);
        request.node_conf = Some(MintlayerNodeClientConfig {
            rpc_url: rpc_endpoint,
            rpc_cookie_file: None,
        });

        let coin = MintlayerCoin::new(&test_ctx(), valid_conf(), request, iguana_policy()).unwrap();

        let sender = coin.address().to_owned();
        let api_server = spawn_withdraw_mock_api(api_listener, sender);

        let rpc_server = thread::spawn(move || {
            let (mut stream, _) = rpc_listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];

            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);

                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let end = pos + 4;
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let len = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);

                    if request.len() >= end + len {
                        break;
                    }
                }
            }

            let body_start = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;

            let body: serde_json::Value = serde_json::from_slice(&request[body_start..]).unwrap();

            assert_eq!(body["method"], "p2p_submit_transaction");
            assert_eq!(body["params"]["options"]["trust_policy"], "Trusted");

            let tx_hex = body["params"]["tx"]
                .as_str()
                .expect("Mintlayer RPC tx must be hexadecimal")
                .to_owned();

            let response_body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": body["id"].clone(),
                "result": null
            })
            .to_string();

            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();

            tx_hex
        });

        let other_secret = [2_u8; 32];
        let other_key_pair = key_pair_from_secret(&other_secret.into()).unwrap();
        let other_pubkey = other_key_pair.public_slice().to_vec();

        let swap_contract_address = None;
        let payment_instructions = None;
        let amount: BigDecimal = "0.5".parse().unwrap();
        let swap_unique_data = b"mintlayer-maker-payment-loopback";

        let args = SendPaymentArgs {
            time_lock_duration: 3600,
            time_lock: TIME_LOCK,
            other_pubkey: &other_pubkey,
            secret_hash: &SECRET_HASH,
            amount,
            swap_contract_address: &swap_contract_address,
            swap_unique_data,
            payment_instructions: &payment_instructions,
            watcher_reward: None,
            wait_for_confirmation_until: 0,
        };

        let result = SwapOps::send_maker_payment(&coin, args).await.unwrap();

        api_server.join().unwrap();
        let submitted_hex = rpc_server.join().unwrap();

        let transaction = match result {
            TransactionEnum::MintlayerTransaction(transaction) => transaction,
            other => panic!("expected Mintlayer transaction from maker payment, got {:?}", other),
        };

        assert_eq!(hex::encode(&transaction.signed_bytes), submitted_hex);

        assert_eq!(
            canonical_transaction_id_from_signed_bytes(&transaction.signed_bytes).unwrap(),
            transaction.transaction_id
        );

        let decoded = mintlayer_sdk::crypto::decode_transaction_lenient(&transaction.signed_bytes).unwrap();

        assert!(
            matches!(decoded.outputs().first(), Some(TxOutput::Htlc(_, _))),
            "maker payment must return a native Mintlayer HTLC transaction"
        );

        let expected_spend_address =
            mintlayer_address_from_compressed_public_key(coin.network(), &other_pubkey).unwrap();

        let local_htlc_key = coin.derive_htlc_key_pair(swap_unique_data);
        let expected_refund_address =
            mintlayer_address_from_compressed_public_key(coin.network(), local_htlc_key.public_slice()).unwrap();

        let expected_output = build_mintlayer_htlc_output(
            50_000_000_000,
            &SECRET_HASH,
            &expected_spend_address,
            &expected_refund_address,
            TIME_LOCK,
            mintlayer_sdk_network(coin.network()),
        )
        .unwrap();

        assert_eq!(decoded.outputs().first(), Some(&expected_output));
    }

    #[tokio::test]
    async fn taker_payment_swapops_builds_broadcasts_and_returns_native_htlc() {
        use mintlayer_sdk::crypto::types::TxOutput;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        const SECRET_HASH: [u8; 20] = [
            0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52,
            0x53, 0x54,
        ];
        const TIME_LOCK: u64 = 1_800_003_600;

        let (api_listener, api_url) = bind_withdraw_mock_api();

        let rpc_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let rpc_endpoint = format!("http://{}", rpc_listener.local_addr().unwrap());

        let mut request = request_with_urls(vec![api_url]);
        request.node_conf = Some(MintlayerNodeClientConfig {
            rpc_url: rpc_endpoint,
            rpc_cookie_file: None,
        });

        let coin = MintlayerCoin::new(&test_ctx(), valid_conf(), request, iguana_policy()).unwrap();

        let sender = coin.address().to_owned();
        let api_server = spawn_withdraw_mock_api(api_listener, sender);

        let rpc_server = thread::spawn(move || {
            let (mut stream, _) = rpc_listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];

            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }

                request.extend_from_slice(&buffer[..read]);

                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let end = pos + 4;
                    let headers = String::from_utf8_lossy(&request[..end]);

                    let len = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);

                    if request.len() >= end + len {
                        break;
                    }
                }
            }

            let body_start = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;

            let body: serde_json::Value = serde_json::from_slice(&request[body_start..]).unwrap();

            assert_eq!(body["method"], "p2p_submit_transaction");
            assert_eq!(body["params"]["options"]["trust_policy"], "Trusted");

            let tx_hex = body["params"]["tx"]
                .as_str()
                .expect("Mintlayer RPC tx must be hexadecimal")
                .to_owned();

            let response_body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": body["id"].clone(),
                "result": null
            })
            .to_string();

            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();

            tx_hex
        });

        let other_secret = [3_u8; 32];
        let other_key_pair = key_pair_from_secret(&other_secret.into()).unwrap();
        let other_pubkey = other_key_pair.public_slice().to_vec();

        let swap_contract_address = None;
        let payment_instructions = None;
        let amount: BigDecimal = "0.5".parse().unwrap();
        let swap_unique_data = b"mintlayer-taker-payment-loopback";

        let args = SendPaymentArgs {
            time_lock_duration: 7200,
            time_lock: TIME_LOCK,
            other_pubkey: &other_pubkey,
            secret_hash: &SECRET_HASH,
            amount,
            swap_contract_address: &swap_contract_address,
            swap_unique_data,
            payment_instructions: &payment_instructions,
            watcher_reward: None,
            wait_for_confirmation_until: 0,
        };

        let result = SwapOps::send_taker_payment(&coin, args).await.unwrap();

        api_server.join().unwrap();
        let submitted_hex = rpc_server.join().unwrap();

        let transaction = match result {
            TransactionEnum::MintlayerTransaction(transaction) => transaction,
            other => panic!("expected Mintlayer transaction from taker payment, got {:?}", other),
        };

        assert_eq!(hex::encode(&transaction.signed_bytes), submitted_hex);

        assert_eq!(
            canonical_transaction_id_from_signed_bytes(&transaction.signed_bytes).unwrap(),
            transaction.transaction_id
        );

        let decoded = mintlayer_sdk::crypto::decode_transaction_lenient(&transaction.signed_bytes).unwrap();

        assert!(
            matches!(decoded.outputs().first(), Some(TxOutput::Htlc(_, _))),
            "taker payment must return a native Mintlayer HTLC transaction"
        );

        let expected_spend_address =
            mintlayer_address_from_compressed_public_key(coin.network(), &other_pubkey).unwrap();

        let local_htlc_key = coin.derive_htlc_key_pair(swap_unique_data);

        let expected_refund_address =
            mintlayer_address_from_compressed_public_key(coin.network(), local_htlc_key.public_slice()).unwrap();

        let expected_output = build_mintlayer_htlc_output(
            50_000_000_000,
            &SECRET_HASH,
            &expected_spend_address,
            &expected_refund_address,
            TIME_LOCK,
            mintlayer_sdk_network(coin.network()),
        )
        .unwrap();

        assert_eq!(decoded.outputs().first(), Some(&expected_output));
    }

    #[tokio::test]
    async fn send_raw_tx_bytes_uses_canonical_txid_and_loopback_broadcaster() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let (api_listener, api_url) = bind_withdraw_mock_api();
        let build_coin = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec![api_url]),
            iguana_policy(),
        )
        .unwrap();
        let sender = build_coin.address().to_owned();
        let api_server = spawn_withdraw_mock_api(api_listener, sender);

        let details = MmCoin::withdraw(&build_coin, withdraw_request(WITHDRAW_RECIPIENT))
            .wait()
            .unwrap();
        api_server.join().unwrap();

        let (signed_bytes, expected_txid) = match details.tx {
            TransactionData::Signed { tx_hex, tx_hash } => (tx_hex.0, tx_hash),
            other => panic!("expected signed Mintlayer transaction, got {:?}", other),
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let expected_hex = hex::encode(&signed_bytes);

        let server = thread::spawn({
            let expected_hex = expected_hex.clone();
            move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];

                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);

                    if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let end = pos + 4;
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_owned)
                            })
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + len {
                            break;
                        }
                    }
                }

                let body_start = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let body: serde_json::Value = serde_json::from_slice(&request[body_start..]).unwrap();
                assert_eq!(body["method"], "p2p_submit_transaction");
                assert_eq!(body["params"]["tx"], expected_hex);
                assert_eq!(body["params"]["options"]["trust_policy"], "Trusted");

                let response_body =
                    serde_json::json!({"jsonrpc":"2.0","id":body["id"].clone(),"result":null}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                )
                .unwrap();
            }
        });

        let mut request = request_with_urls(vec![concat!("https", "://api.example").into()]);
        request.node_conf = Some(MintlayerNodeClientConfig {
            rpc_url: endpoint,
            rpc_cookie_file: None,
        });
        let coin = MintlayerCoin::new(&test_ctx(), valid_conf(), request, iguana_policy()).unwrap();

        let returned_txid = coin.send_raw_tx_bytes(&signed_bytes).compat().await.unwrap();
        assert_eq!(returned_txid, expected_txid);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn transaction_data_broadcast_orchestrator_preserves_bytes_and_canonical_txid() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let expected_bytes = vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let expected_hex = hex::encode(&expected_bytes);
        let expected_txid = "11".repeat(32);

        let server = thread::spawn({
            let expected_hex = expected_hex.clone();
            move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(header_pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header_end = header_pos + 4;
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_owned)
                            })
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= header_end + length {
                            break;
                        }
                    }
                }
                let body_start = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let body: serde_json::Value = serde_json::from_slice(&request[body_start..]).unwrap();
                assert_eq!(body["method"], "p2p_submit_transaction");
                assert_eq!(body["params"]["tx"], expected_hex);
                assert_eq!(body["params"]["options"]["trust_policy"], "Trusted");
                let response_body =
                    serde_json::json!({"jsonrpc":"2.0","id":body["id"].clone(),"result":null}).to_string();
                write!(stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(), response_body).unwrap();
            }
        });

        let mut request = request_with_urls(vec![concat!("https", "://api.example").into()]);
        request.node_conf = Some(MintlayerNodeClientConfig {
            rpc_url: endpoint,
            rpc_cookie_file: None,
        });
        let coin = MintlayerCoin::new(&test_ctx(), valid_conf(), request, iguana_policy()).unwrap();
        let tx = TransactionData::new_signed(expected_bytes.into(), expected_txid.clone());
        let returned_txid = broadcast_mintlayer_transaction_data(&coin, &tx).await.unwrap();
        assert_eq!(returned_txid, expected_txid);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn node_rpc_cookie_auth_is_reloaded_and_sent_on_wire() {
        use std::fs;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;
        use std::time::{SystemTime, UNIX_EPOCH};

        fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let end = pos + 4;
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let len = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + len {
                        break;
                    }
                }
            }
            request
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());

        let server = thread::spawn(move || {
            for expected_auth in [
                "Basic Y29va2llLXVzZXI6Y29va2llLXBhc3M=",
                "Basic cm90YXRlZC11c2VyOnJvdGF0ZWQtcGFzcw==",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                let header_end = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);

                assert!(
                    headers
                        .lines()
                        .any(|line| line.eq_ignore_ascii_case(&format!("authorization: {expected_auth}"))),
                    "expected Authorization header was not sent"
                );

                let body: serde_json::Value = serde_json::from_slice(&request[header_end..]).unwrap();
                assert_eq!(body["method"], "node_version");
                let response =
                    serde_json::json!({"jsonrpc":"2.0","id":body["id"].clone(),"result":"1.4.0"}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )
                .unwrap();
            }
        });

        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let cookie_path = std::env::temp_dir().join(format!("kdf-ml-rpc-cookie-{}-{unique}", std::process::id()));
        fs::write(&cookie_path, "cookie-user:cookie-pass").unwrap();

        let mut request = request_with_urls(vec![concat!("https", "://api.example").into()]);
        request.node_conf = Some(MintlayerNodeClientConfig {
            rpc_url: endpoint,
            rpc_cookie_file: Some(cookie_path.to_string_lossy().into_owned()),
        });
        let coin = MintlayerCoin::new(&test_ctx(), valid_conf(), request, iguana_policy()).unwrap();

        let first_client = coin.node_client().unwrap().unwrap();
        assert_eq!(first_client.node_version().await.unwrap(), "1.4.0");

        fs::write(&cookie_path, "rotated-user:rotated-pass").unwrap();
        let rotated_client = coin.node_client().unwrap().unwrap();
        assert_eq!(rotated_client.node_version().await.unwrap(), "1.4.0");

        fs::write(&cookie_path, "invalid-cookie").unwrap();
        assert!(coin
            .node_client()
            .unwrap_err()
            .contains("Invalid Mintlayer node RPC cookie format"));

        fs::remove_file(cookie_path).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn node_rpc_configuration_is_optional_and_loopback_only() {
        let without_node = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec![concat!("https", "://api.example").into()]),
            iguana_policy(),
        )
        .unwrap();
        assert_eq!(without_node.node_rpc_url(), None);
        assert!(without_node.node_client().unwrap().is_none());

        let mut with_node = request_with_urls(vec![concat!("https", "://api.example").into()]);
        with_node.node_conf = Some(MintlayerNodeClientConfig {
            rpc_url: "http://127.0.0.1:3030/".into(),
            rpc_cookie_file: None,
        });
        let with_node = MintlayerCoin::new(&test_ctx(), valid_conf(), with_node, iguana_policy()).unwrap();
        assert_eq!(with_node.node_rpc_url(), Some("http://127.0.0.1:3030"));
        assert!(with_node.node_client().unwrap().is_some());

        for forbidden in [
            "https://127.0.0.1:3030",
            "http://example.com:3030",
            "http://user:pass@127.0.0.1:3030",
        ] {
            let mut request = request_with_urls(vec![concat!("https", "://api.example").into()]);
            request.node_conf = Some(MintlayerNodeClientConfig {
                rpc_url: forbidden.into(),
                rpc_cookie_file: None,
            });
            assert!(MintlayerCoin::new(&test_ctx(), valid_conf(), request, iguana_policy()).is_err());
        }
    }

    #[test]
    fn convert_address_info_to_coin_balance() {
        let address_info = MintlayerAddressInfo {
            coin_balance: crate::mintlayer::MintlayerAmount {
                atoms: "1250000000000".into(),
                decimal: "12.5".into(),
            },
            locked_coin_balance: crate::mintlayer::MintlayerAmount {
                atoms: "250000000000".into(),
                decimal: "2.5".into(),
            },
            transaction_history: Vec::new(),
            tokens: Vec::new(),
        };

        let balance = coin_balance_from_address_info(address_info).unwrap();

        assert_eq!(balance.spendable, "12.5".parse().unwrap());
        assert_eq!(balance.unspendable, "2.5".parse().unwrap());
    }

    #[test]
    fn reject_inconsistent_address_info_balance() {
        let address_info = MintlayerAddressInfo {
            coin_balance: crate::mintlayer::MintlayerAmount {
                atoms: "1250000000000".into(),
                decimal: "12.6".into(),
            },
            locked_coin_balance: crate::mintlayer::MintlayerAmount {
                atoms: "0".into(),
                decimal: "0".into(),
            },
            transaction_history: Vec::new(),
            tokens: Vec::new(),
        };

        assert!(matches!(
            coin_balance_from_address_info(address_info),
            Err(BalanceError::InvalidResponse(_))
        ));
    }

    #[test]
    fn converts_mintlayer_decimal_amounts_to_atoms_exactly() {
        let one: BigDecimal = "1".parse().unwrap();
        let fractional: BigDecimal = "1.23456789012".parse().unwrap();
        let trailing_zero: BigDecimal = "0.100000000000".parse().unwrap();
        let too_precise: BigDecimal = "0.000000000001".parse().unwrap();

        assert_eq!(mintlayer_atoms_from_decimal(&one).unwrap(), 100_000_000_000);
        assert_eq!(mintlayer_atoms_from_decimal(&fractional).unwrap(), 123_456_789_012);
        assert_eq!(mintlayer_atoms_from_decimal(&trailing_zero).unwrap(), 10_000_000_000);
        assert!(mintlayer_atoms_from_decimal(&too_precise).is_err());
    }

    #[test]
    fn maps_every_kdf_mintlayer_network_to_sdk() {
        assert_eq!(mintlayer_sdk_network(MintlayerNetwork::Mainnet), SdkNetwork::Mainnet);
        assert_eq!(mintlayer_sdk_network(MintlayerNetwork::Testnet), SdkNetwork::Testnet);
        assert_eq!(mintlayer_sdk_network(MintlayerNetwork::Regtest), SdkNetwork::Regtest);
        assert_eq!(mintlayer_sdk_network(MintlayerNetwork::Signet), SdkNetwork::Signet);
    }

    #[test]
    fn mmcoin_is_always_wallet_only() {
        let ctx = test_ctx();
        let coin = MintlayerCoin::new(
            &ctx,
            valid_conf(),
            request_with_urls(vec![concat!("https", "://api.example").into()]),
            iguana_policy(),
        )
        .unwrap();

        assert!(MmCoin::wallet_only(&coin, &ctx));
        assert!(!MmCoin::is_coin_protocol_supported(&coin, &None, None, 0, false));
    }

    #[test]
    fn mmcoin_validates_local_address() {
        let ctx = test_ctx();
        let coin = MintlayerCoin::new(
            &ctx,
            valid_conf(),
            request_with_urls(vec![concat!("https", "://api.example").into()]),
            iguana_policy(),
        )
        .unwrap();

        let valid = MmCoin::validate_address(&coin, coin.address());
        assert!(valid.is_valid);
        assert!(valid.reason.is_none());

        let invalid = MmCoin::validate_address(&coin, "not-a-mintlayer-address");
        assert!(!invalid.is_valid);
        assert!(invalid.reason.is_some());
    }

    #[test]
    fn htlc_public_key_matches_local_identity() {
        let ctx = test_ctx();
        let coin = MintlayerCoin::new(
            &ctx,
            valid_conf(),
            request_with_urls(vec![concat!("https", "://api.example").into()]),
            iguana_policy(),
        )
        .unwrap();

        let htlc_public_key = SwapOps::derive_htlc_pubkey(&coin, b"test-swap");
        assert_eq!(htlc_public_key.as_slice(), coin.public_key());
        assert!(SwapOps::validate_other_pubkey(&coin, &htlc_public_key).is_ok());
        assert!(SwapOps::validate_other_pubkey(&coin, &[0_u8; 32]).is_err());
    }

    #[test]
    fn build_valid_coin() {
        let request = request_with_urls(vec![
            concat!("https", "://api.example").into(),
            concat!("http", "://127.0.0.1:3000").into(),
        ]);

        let coin = MintlayerCoin::new(&test_ctx(), valid_conf(), request, iguana_policy()).unwrap();

        assert_eq!(coin.ticker(), "ML");
        assert_eq!(coin.network(), MintlayerNetwork::Mainnet);
        assert_eq!(coin.decimals(), MINTLAYER_DECIMALS);
        assert_eq!(coin.required_confirmations(), 2);
        assert_eq!(coin.api_urls().len(), 2);
        assert_eq!(coin.api_client().api_urls(), coin.api_urls());
        assert_eq!(
            hex::encode(coin.public_key()),
            "031b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
        );
        assert_eq!(
            coin.address(),
            mintlayer_address_from_compressed_public_key(MintlayerNetwork::Mainnet, coin.public_key()).unwrap()
        );
        assert!(matches!(coin.derivation_method(), DerivationMethodResponse::Iguana));
    }

    #[test]
    fn build_global_hd_identity_at_default_address_path() {
        let ctx = MmCtxBuilder::default().into_mm_arc();
        let crypto_ctx = CryptoCtx::init_with_global_hd_account(ctx.clone(), TEST_MNEMONIC).unwrap();
        let global_hd_account = match crypto_ctx.key_pair_policy() {
            KeyPairPolicy::GlobalHDAccount(account) => account.clone(),
            KeyPairPolicy::Iguana => panic!("expected GlobalHDAccount policy"),
        };

        let expected_path = mintlayer_derivation_path(MintlayerNetwork::Mainnet, 0, Bip44Chain::External, 0).unwrap();
        let expected_secret = global_hd_account.derive_secp256k1_secret(&expected_path).unwrap();
        let expected_key_pair = key_pair_from_secret(&expected_secret.take()).unwrap();
        let expected_address =
            mintlayer_address_from_compressed_public_key(MintlayerNetwork::Mainnet, expected_key_pair.public_slice())
                .unwrap();

        let coin = MintlayerCoin::new(
            &ctx,
            valid_conf(),
            request_with_urls(vec![concat!("https", "://api.example").into()]),
            PrivKeyBuildPolicy::GlobalHDAccount(global_hd_account),
        )
        .unwrap();

        assert_eq!(coin.public_key(), expected_key_pair.public_slice());
        assert_eq!(coin.address(), expected_address);
        assert!(matches!(
            coin.derivation_method(),
            DerivationMethodResponse::HDWallet(path) if path == &expected_path.to_string()
        ));
    }

    #[test]
    fn debug_redacts_private_key() {
        let coin = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec![concat!("https", "://api.example").into()]),
            iguana_policy(),
        )
        .unwrap();

        let debug = format!("{coin:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(&"01".repeat(32)));
    }

    #[test]
    fn reject_trezor_private_key_policy() {
        let result = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec![concat!("https", "://api.example").into()]),
            PrivKeyBuildPolicy::Trezor,
        );

        assert!(matches!(
            result,
            Err(MintlayerCoinBuildError::UnsupportedPrivKeyPolicy { policy: "Trezor" })
        ));
    }

    #[test]
    fn activation_confirmations_override_configuration() {
        let mut request = request_with_urls(vec![concat!("https", "://api.example").into()]);
        request.required_confirmations = Some(5);

        let coin = MintlayerCoin::new(&test_ctx(), valid_conf(), request, iguana_policy()).unwrap();

        assert_eq!(coin.required_confirmations(), 5);
    }

    #[test]
    fn reject_zero_confirmations_in_setter() {
        let request = request_with_urls(vec![concat!("https", "://api.example").into()]);
        let coin = MintlayerCoin::new(&test_ctx(), valid_conf(), request, iguana_policy()).unwrap();

        let result = coin.set_required_confirmations(0);

        assert_eq!(result, Err(MintlayerCoinBuildError::InvalidRequiredConfirmations));
        assert_eq!(coin.required_confirmations(), 2);
    }

    #[test]
    fn accept_matching_api_genesis_block_id_case_insensitively() {
        let expected = "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let actual = expected.to_uppercase();

        assert!(validate_api_genesis_block_id(expected, &actual).is_ok());
    }

    #[test]
    fn reject_invalid_api_genesis_block_id() {
        let expected = "a".repeat(64);

        let error = validate_api_genesis_block_id(&expected, "not-a-block-id").unwrap_err();

        assert!(matches!(
            error,
            MintlayerNetworkValidationError::InvalidGenesisBlockId { .. }
        ));
    }

    #[test]
    fn reject_unexpected_api_genesis_block_id() {
        let expected = "a".repeat(64);
        let actual = "b".repeat(64);

        let error = validate_api_genesis_block_id(&expected, &actual).unwrap_err();

        assert!(matches!(
            error,
            MintlayerNetworkValidationError::UnexpectedGenesisBlockId {
                expected: error_expected,
                actual: error_actual,
            } if error_expected == expected && error_actual == actual
        ));
    }

    #[test]
    fn reject_invalid_genesis_block_id() {
        let mut conf = valid_conf();
        conf.genesis_block_id = "not-a-block-id".into();

        let result = MintlayerCoin::new(
            &test_ctx(),
            conf,
            request_with_urls(vec![concat!("https", "://api.example").into()]),
            iguana_policy(),
        );

        assert!(matches!(result, Err(MintlayerCoinBuildError::InvalidGenesisBlockId)));
    }

    #[test]
    fn reject_remote_plain_http() {
        let result = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec![concat!("http", "://api.example").into()]),
            iguana_policy(),
        );

        assert!(matches!(
            result,
            Err(MintlayerCoinBuildError::InsecureRemoteApiUrl { index: 0 })
        ));
    }

    #[test]
    fn reject_url_credentials() {
        let result = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec![concat!("https", "://user:password@api.example").into()]),
            iguana_policy(),
        );

        assert!(matches!(
            result,
            Err(MintlayerCoinBuildError::ApiUrlCredentialsNotAllowed { index: 0 })
        ));
    }

    #[test]
    fn reject_duplicate_urls_after_normalization() {
        let result = MintlayerCoin::new(
            &test_ctx(),
            valid_conf(),
            request_with_urls(vec![
                concat!("https", "://api.example").into(),
                concat!("https", "://api.example/").into(),
            ]),
            iguana_policy(),
        );

        assert!(matches!(
            result,
            Err(MintlayerCoinBuildError::DuplicateApiUrl { index: 1 })
        ));
    }
}
