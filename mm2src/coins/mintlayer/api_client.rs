use crate::mintlayer::{MintlayerAddressInfo, MintlayerChainTip, MintlayerUtxo};
use async_std::prelude::FutureExt;
use async_trait::async_trait;
use compatible_time::Duration;
use http::StatusCode;
use mm2_net::transport::slurp_url;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use thiserror::Error;
use url::Url;

const MINTLAYER_API_VERSION_PATH: &[&str] = &["api", "v2"];
const MINTLAYER_API_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ERROR_BODY_PREVIEW: usize = 512;

#[derive(Clone, Debug, Error, PartialEq)]
pub enum MintlayerEndpointError {
    #[error("Request timed out")]
    Timeout,
    #[error("Transport error: {0}")]
    Transport(String),
    #[error("HTTP status {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("Invalid JSON response: {0}")]
    InvalidResponse(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MintlayerEndpointFailure {
    pub endpoint: String,
    pub error: MintlayerEndpointError,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum MintlayerApiError {
    #[error("Mintlayer API path segment cannot be empty")]
    EmptyPathSegment,
    #[error("Cannot construct Mintlayer API URL from '{base_url}'")]
    EndpointUrlConstruction { base_url: String },
    #[error("All Mintlayer API endpoints failed: {failures:?}")]
    AllEndpointsFailed { failures: Vec<MintlayerEndpointFailure> },
}

#[async_trait]
pub trait MintlayerHttpTransport: Send + Sync + 'static {
    async fn get(&self, url: &str) -> Result<(StatusCode, Vec<u8>), String>;
}

#[derive(Debug, Default)]
pub struct KdfMintlayerHttpTransport;

#[async_trait]
impl MintlayerHttpTransport for KdfMintlayerHttpTransport {
    async fn get(&self, url: &str) -> Result<(StatusCode, Vec<u8>), String> {
        slurp_url(url)
            .await
            .map(|(status, _headers, body)| (status, body))
            .map_err(|error| error.into_inner().to_string())
    }
}

pub struct MintlayerApiClientGeneric<T> {
    api_urls: Vec<Url>,
    transport: Arc<T>,
    timeout: Duration,
}

pub type MintlayerApiClient = MintlayerApiClientGeneric<KdfMintlayerHttpTransport>;

impl<T> Clone for MintlayerApiClientGeneric<T> {
    fn clone(&self) -> Self {
        MintlayerApiClientGeneric {
            api_urls: self.api_urls.clone(),
            transport: Arc::clone(&self.transport),
            timeout: self.timeout,
        }
    }
}

impl MintlayerApiClient {
    pub fn new(api_urls: Vec<Url>) -> Self {
        MintlayerApiClientGeneric {
            api_urls,
            transport: Arc::new(KdfMintlayerHttpTransport),
            timeout: MINTLAYER_API_TIMEOUT,
        }
    }
}

impl<T> MintlayerApiClientGeneric<T>
where
    T: MintlayerHttpTransport,
{
    #[cfg(test)]
    fn with_transport(api_urls: Vec<Url>, transport: Arc<T>, timeout: Duration) -> Self {
        MintlayerApiClientGeneric {
            api_urls,
            transport,
            timeout,
        }
    }

    pub fn api_urls(&self) -> &[Url] {
        &self.api_urls
    }

    pub async fn chain_tip(&self) -> Result<MintlayerChainTip, MintlayerApiError> {
        self.get_json(&["chain", "tip"]).await
    }

    pub async fn address_info(&self, address: &str) -> Result<MintlayerAddressInfo, MintlayerApiError> {
        validate_path_segment(address)?;
        self.get_json(&["address", address]).await
    }

    pub async fn spendable_utxos(&self, address: &str) -> Result<Vec<MintlayerUtxo>, MintlayerApiError> {
        validate_path_segment(address)?;
        self.get_json(&["address", address, "spendable-utxos"]).await
    }

    async fn get_json<R>(&self, endpoint_path: &[&str]) -> Result<R, MintlayerApiError>
    where
        R: DeserializeOwned,
    {
        for segment in endpoint_path {
            validate_path_segment(segment)?;
        }

        let mut failures = Vec::with_capacity(self.api_urls.len());

        for base_url in &self.api_urls {
            let endpoint = build_endpoint_url(base_url, endpoint_path)?;
            let endpoint_string = endpoint.to_string();

            let response = self.transport.get(endpoint.as_str()).timeout(self.timeout).await;

            let (status, body) = match response {
                Err(_) => {
                    failures.push(MintlayerEndpointFailure {
                        endpoint: endpoint_string,
                        error: MintlayerEndpointError::Timeout,
                    });
                    continue;
                },
                Ok(Err(error)) => {
                    failures.push(MintlayerEndpointFailure {
                        endpoint: endpoint_string,
                        error: MintlayerEndpointError::Transport(error),
                    });
                    continue;
                },
                Ok(Ok(response)) => response,
            };

            if !status.is_success() {
                failures.push(MintlayerEndpointFailure {
                    endpoint: endpoint_string,
                    error: MintlayerEndpointError::HttpStatus {
                        status: status.as_u16(),
                        body: response_body_preview(&body),
                    },
                });
                continue;
            }

            match serde_json::from_slice(&body) {
                Ok(response) => return Ok(response),
                Err(error) => failures.push(MintlayerEndpointFailure {
                    endpoint: endpoint_string,
                    error: MintlayerEndpointError::InvalidResponse(error.to_string()),
                }),
            }
        }

        Err(MintlayerApiError::AllEndpointsFailed { failures })
    }
}

fn validate_path_segment(segment: &str) -> Result<(), MintlayerApiError> {
    if segment.is_empty() {
        return Err(MintlayerApiError::EmptyPathSegment);
    }

    Ok(())
}

fn build_endpoint_url(base_url: &Url, endpoint_path: &[&str]) -> Result<Url, MintlayerApiError> {
    let mut endpoint = base_url.clone();

    {
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| MintlayerApiError::EndpointUrlConstruction {
                base_url: base_url.to_string(),
            })?;

        segments.pop_if_empty();

        for segment in MINTLAYER_API_VERSION_PATH {
            segments.push(segment);
        }

        for segment in endpoint_path {
            segments.push(segment);
        }
    }

    Ok(endpoint)
}

fn response_body_preview(body: &[u8]) -> String {
    let preview_length = body.len().min(MAX_ERROR_BODY_PREVIEW);
    let mut preview = String::from_utf8_lossy(&body[..preview_length]).into_owned();

    if body.len() > MAX_ERROR_BODY_PREVIEW {
        preview.push_str("...");
    }

    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::block_on;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    type MockResponse = Result<(StatusCode, Vec<u8>), String>;

    struct MockTransport {
        responses: Mutex<VecDeque<MockResponse>>,
        requested_urls: Mutex<Vec<String>>,
    }

    impl MockTransport {
        fn new(responses: Vec<MockResponse>) -> Self {
            MockTransport {
                responses: Mutex::new(responses.into()),
                requested_urls: Mutex::new(Vec::new()),
            }
        }

        fn requested_urls(&self) -> Vec<String> {
            self.requested_urls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MintlayerHttpTransport for MockTransport {
        async fn get(&self, url: &str) -> Result<(StatusCode, Vec<u8>), String> {
            self.requested_urls.lock().unwrap().push(url.to_string());

            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("No mocked response configured".into()))
        }
    }

    fn api_url(host: &str) -> Url {
        Url::parse(&format!("https://{host}")).unwrap()
    }

    fn client_with_responses(
        api_urls: Vec<Url>,
        responses: Vec<MockResponse>,
    ) -> (MintlayerApiClientGeneric<MockTransport>, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new(responses));
        let client =
            MintlayerApiClientGeneric::with_transport(api_urls, Arc::clone(&transport), Duration::from_secs(1));

        (client, transport)
    }

    #[test]
    fn deserialize_chain_tip_from_first_endpoint() {
        let response = br#"{
            "block_height": 680824,
            "block_id": "635ce5bab5992e1c6a32cc9983ec4d295b1c89cc05afdf328f73041b9331a8d1"
        }"#
        .to_vec();

        let (client, transport) =
            client_with_responses(vec![api_url("api-1.example")], vec![Ok((StatusCode::OK, response))]);

        let tip = block_on(client.chain_tip()).unwrap();

        assert_eq!(tip.block_height, 680824);
        assert_eq!(
            transport.requested_urls(),
            vec!["https://api-1.example/api/v2/chain/tip"]
        );
    }

    #[test]
    fn fail_over_after_unsuccessful_http_status() {
        let valid_response = br#"{
            "block_height": 680825,
            "block_id": "735ce5bab5992e1c6a32cc9983ec4d295b1c89cc05afdf328f73041b9331a8d1"
        }"#
        .to_vec();

        let (client, transport) = client_with_responses(
            vec![api_url("api-1.example"), api_url("api-2.example")],
            vec![
                Ok((StatusCode::SERVICE_UNAVAILABLE, b"maintenance".to_vec())),
                Ok((StatusCode::OK, valid_response)),
            ],
        );

        let tip = block_on(client.chain_tip()).unwrap();

        assert_eq!(tip.block_height, 680825);
        assert_eq!(
            transport.requested_urls(),
            vec![
                "https://api-1.example/api/v2/chain/tip",
                "https://api-2.example/api/v2/chain/tip",
            ]
        );
    }

    #[test]
    fn fail_over_after_transport_error() {
        let valid_response = br#"{
            "block_height": 680826,
            "block_id": "835ce5bab5992e1c6a32cc9983ec4d295b1c89cc05afdf328f73041b9331a8d1"
        }"#
        .to_vec();

        let (client, transport) = client_with_responses(
            vec![api_url("api-1.example"), api_url("api-2.example")],
            vec![Err("connection refused".into()), Ok((StatusCode::OK, valid_response))],
        );

        let tip = block_on(client.chain_tip()).unwrap();

        assert_eq!(tip.block_height, 680826);
        assert_eq!(transport.requested_urls().len(), 2);
    }

    #[test]
    fn report_invalid_json_after_all_endpoints_fail() {
        let (client, _transport) = client_with_responses(
            vec![api_url("api-1.example")],
            vec![Ok((StatusCode::OK, b"{invalid-json".to_vec()))],
        );

        let error = block_on(client.chain_tip()).unwrap_err();

        match error {
            MintlayerApiError::AllEndpointsFailed { failures } => {
                assert_eq!(failures.len(), 1);
                assert!(matches!(failures[0].error, MintlayerEndpointError::InvalidResponse(_)));
            },
            unexpected => panic!("Unexpected error: {:?}", unexpected),
        }
    }

    #[test]
    fn preserve_reverse_proxy_prefix() {
        let base_url = Url::parse("https://api.example/mintlayer").unwrap();
        let response = br#"{
            "block_height": 680827,
            "block_id": "935ce5bab5992e1c6a32cc9983ec4d295b1c89cc05afdf328f73041b9331a8d1"
        }"#
        .to_vec();

        let (client, transport) = client_with_responses(vec![base_url], vec![Ok((StatusCode::OK, response))]);

        block_on(client.chain_tip()).unwrap();

        assert_eq!(
            transport.requested_urls(),
            vec!["https://api.example/mintlayer/api/v2/chain/tip"]
        );
    }

    #[test]
    fn request_spendable_utxos() {
        let response = br#"[{
            "outpoint": {
                "source_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
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
        }]"#
        .to_vec();

        let (client, transport) =
            client_with_responses(vec![api_url("api.example")], vec![Ok((StatusCode::OK, response))]);

        let utxos = block_on(client.spendable_utxos("mtc1qexample")).unwrap();

        assert_eq!(utxos.len(), 1);
        assert_eq!(utxos[0].outpoint.index, 1);
        assert_eq!(
            transport.requested_urls(),
            vec!["https://api.example/api/v2/address/mtc1qexample/spendable-utxos"]
        );
    }

    #[test]
    fn reject_empty_address_without_requesting_endpoint() {
        let (client, transport) = client_with_responses(vec![api_url("api.example")], Vec::new());

        let error = block_on(client.address_info("")).unwrap_err();

        assert_eq!(error, MintlayerApiError::EmptyPathSegment);
        assert!(transport.requested_urls().is_empty());
    }

    #[test]
    fn truncate_http_error_body() {
        let large_body = vec![b'x'; MAX_ERROR_BODY_PREVIEW + 100];
        let (client, _transport) = client_with_responses(
            vec![api_url("api.example")],
            vec![Ok((StatusCode::INTERNAL_SERVER_ERROR, large_body))],
        );

        let error = block_on(client.chain_tip()).unwrap_err();

        match error {
            MintlayerApiError::AllEndpointsFailed { failures } => match &failures[0].error {
                MintlayerEndpointError::HttpStatus { body, .. } => {
                    assert_eq!(body.len(), MAX_ERROR_BODY_PREVIEW + 3);
                    assert!(body.ends_with("..."));
                },
                unexpected => panic!("Unexpected endpoint error: {:?}", unexpected),
            },
            unexpected => panic!("Unexpected API error: {:?}", unexpected),
        }
    }
}
