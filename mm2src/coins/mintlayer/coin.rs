use crate::mintlayer::{MintlayerActivationRequest, MintlayerCoinConf, MintlayerNetwork};
use derive_more::Display;
use std::collections::HashSet;
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
}

#[derive(Clone, Debug)]
pub struct MintlayerCoin(Arc<MintlayerCoinImpl>);

impl Deref for MintlayerCoin {
    type Target = MintlayerCoinImpl;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct MintlayerCoinImpl {
    conf: MintlayerCoinConf,
    api_urls: Vec<Url>,
    tx_history: bool,
    required_confirmations: AtomicU64,
}

impl MintlayerCoin {
    pub fn new(conf: MintlayerCoinConf, request: MintlayerActivationRequest) -> Result<Self, MintlayerCoinBuildError> {
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

        let required_confirmations = request.required_confirmations.unwrap_or(conf.required_confirmations);

        if required_confirmations == 0 {
            return Err(MintlayerCoinBuildError::InvalidRequiredConfirmations);
        }

        Ok(MintlayerCoin(Arc::new(MintlayerCoinImpl {
            conf,
            api_urls,
            tx_history: request.tx_history,
            required_confirmations: AtomicU64::new(required_confirmations),
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

fn validate_genesis_block_id(genesis_block_id: &str) -> Result<(), MintlayerCoinBuildError> {
    if genesis_block_id.len() != 64 {
        return Err(MintlayerCoinBuildError::InvalidGenesisBlockId);
    }

    let bytes = hex::decode(genesis_block_id).map_err(|_| MintlayerCoinBuildError::InvalidGenesisBlockId)?;

    if bytes.len() != 32 {
        return Err(MintlayerCoinBuildError::InvalidGenesisBlockId);
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

    #[test]
    fn build_valid_coin() {
        let request = request_with_urls(vec![
            concat!("https", "://api.example").into(),
            concat!("http", "://127.0.0.1:3000").into(),
        ]);

        let coin = MintlayerCoin::new(valid_conf(), request).unwrap();

        assert_eq!(coin.ticker(), "ML");
        assert_eq!(coin.network(), MintlayerNetwork::Mainnet);
        assert_eq!(coin.decimals(), MINTLAYER_DECIMALS);
        assert_eq!(coin.required_confirmations(), 2);
        assert_eq!(coin.api_urls().len(), 2);
    }

    #[test]
    fn activation_confirmations_override_configuration() {
        let mut request = request_with_urls(vec![concat!("https", "://api.example").into()]);
        request.required_confirmations = Some(5);

        let coin = MintlayerCoin::new(valid_conf(), request).unwrap();

        assert_eq!(coin.required_confirmations(), 5);
    }

    #[test]
    fn reject_zero_confirmations_in_setter() {
        let request = request_with_urls(vec![concat!("https", "://api.example").into()]);
        let coin = MintlayerCoin::new(valid_conf(), request).unwrap();

        let result = coin.set_required_confirmations(0);

        assert_eq!(result, Err(MintlayerCoinBuildError::InvalidRequiredConfirmations));
        assert_eq!(coin.required_confirmations(), 2);
    }

    #[test]
    fn reject_invalid_genesis_block_id() {
        let mut conf = valid_conf();
        conf.genesis_block_id = "not-a-block-id".into();

        let result = MintlayerCoin::new(conf, request_with_urls(vec![concat!("https", "://api.example").into()]));

        assert!(matches!(result, Err(MintlayerCoinBuildError::InvalidGenesisBlockId)));
    }

    #[test]
    fn reject_remote_plain_http() {
        let result = MintlayerCoin::new(
            valid_conf(),
            request_with_urls(vec![concat!("http", "://api.example").into()]),
        );

        assert!(matches!(
            result,
            Err(MintlayerCoinBuildError::InsecureRemoteApiUrl { index: 0 })
        ));
    }

    #[test]
    fn reject_url_credentials() {
        let result = MintlayerCoin::new(
            valid_conf(),
            request_with_urls(vec![concat!("https", "://user:password@api.example").into()]),
        );

        assert!(matches!(
            result,
            Err(MintlayerCoinBuildError::ApiUrlCredentialsNotAllowed { index: 0 })
        ));
    }

    #[test]
    fn reject_duplicate_urls_after_normalization() {
        let result = MintlayerCoin::new(
            valid_conf(),
            request_with_urls(vec![
                concat!("https", "://api.example").into(),
                concat!("https", "://api.example/").into(),
            ]),
        );

        assert!(matches!(
            result,
            Err(MintlayerCoinBuildError::DuplicateApiUrl { index: 1 })
        ));
    }
}
