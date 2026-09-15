use crate::coin_errors::{AddressFromPubkeyError, MyAddressError};
use crate::hd_wallet::HDAddressSelector;
use crate::mintlayer::{
    mintlayer_address_from_compressed_public_key, mintlayer_derivation_path, MintlayerActivationRequest,
    MintlayerAddressInfo, MintlayerApiClient, MintlayerApiError, MintlayerChainTip, MintlayerCoinConf,
    MintlayerNetwork, MintlayerUtxo,
};
use crate::{
    BalanceError, BalanceFut, CoinBalance, ConfirmPaymentInput, DerivationMethodResponse, MarketCoinOps,
    PrivKeyBuildPolicy, SignatureError, SignatureResult, TransactionEnum, TransactionErr, TransactionResult,
    TxMarshalingErr, UnexpectedDerivationMethod, VerificationError, VerificationResult, WaitForHTLCTxSpendArgs,
};
use async_trait::async_trait;
use common::executor::abortable_queue::AbortableQueue;
use common::executor::AbortableSystem;
use crypto::privkey::key_pair_from_secret;
use crypto::Bip44Chain;
use derive_more::Display;
use futures::{FutureExt, TryFutureExt};
use futures01::Future;
use keys::KeyPair;
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

    fn send_raw_tx(&self, _tx: &str) -> Box<dyn Future<Item = String, Error = String> + Send> {
        Box::new(futures01::future::err(
            "Mintlayer raw transaction broadcast is not implemented".to_string(),
        ))
    }

    fn send_raw_tx_bytes(&self, _tx: &[u8]) -> Box<dyn Future<Item = String, Error = String> + Send> {
        Box::new(futures01::future::err(
            "Mintlayer raw transaction broadcast is not implemented".to_string(),
        ))
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
    use mm2_core::mm_ctx::{MmArc, MmCtxBuilder};

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
        }
    }

    fn iguana_policy() -> PrivKeyBuildPolicy {
        PrivKeyBuildPolicy::IguanaPrivKey([1_u8; 32].into())
    }

    fn test_ctx() -> MmArc {
        MmCtxBuilder::default().into_mm_arc()
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
