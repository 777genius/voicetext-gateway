use std::fmt;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Client, Response, StatusCode, Url};
use thiserror::Error;
use voicetext_speech::application::batch_capabilities::BatchCapabilityDescriptor;
use voicetext_speech::application::ports::{
    BatchReadableSegment, BatchRecognitionRequest, BatchRecognitionResult, BatchRecognizer,
    BatchSegment, BoxFuture, ProviderOperationKind, ProviderReference, RecognitionFailure,
};

use super::batch_capabilities::{self, CAPABILITIES};
use super::dto::{self, ParsedBatchResult};
use super::timeline;

pub(super) const CONTRACT_VERSION: u16 = 2;
pub(super) const PROVIDER: &str = "deepgram";
pub(super) const MODEL: &str = "nova-3";
pub(super) const LANGUAGE: &str = "multi";
const MAX_SUCCESS_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const MAX_RETRY_AFTER_MILLIS: u64 = 30_000;

/// Invalid injected configuration for a Deepgram batch adapter.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DeepgramConfigurationError {
    #[error("Deepgram API key must be non-empty and unpadded")]
    InvalidApiKey,
    #[error("Deepgram API key cannot be represented as an authorization header")]
    InvalidAuthorizationHeader,
}

/// Deepgram Nova-3 implementation of the provider-neutral paid batch port.
pub struct DeepgramBatchRecognizer {
    client: Client,
    authorization: HeaderValue,
    endpoint: Url,
}

impl DeepgramBatchRecognizer {
    /// Creates an adapter from explicitly injected transport configuration.
    ///
    /// # Errors
    ///
    /// Returns [`DeepgramConfigurationError`] for an empty, padded, or invalid API key.
    pub fn new(
        client: Client,
        api_key: &str,
        endpoint: Url,
    ) -> Result<Self, DeepgramConfigurationError> {
        if api_key.is_empty() || api_key.trim() != api_key {
            return Err(DeepgramConfigurationError::InvalidApiKey);
        }
        let mut authorization = HeaderValue::from_str(&format!("Token {api_key}"))
            .map_err(|_| DeepgramConfigurationError::InvalidAuthorizationHeader)?;
        authorization.set_sensitive(true);
        Ok(Self {
            client,
            authorization,
            endpoint,
        })
    }

    async fn recognize_inner(
        &self,
        request: BatchRecognitionRequest,
    ) -> Result<BatchRecognitionResult, RecognitionFailure> {
        if !profile_matches(&request) || !batch_capabilities::matches(&request) {
            return Err(known_not_accepted(
                false,
                "BATCH_PROFILE_MISMATCH",
                None,
                None,
            ));
        }

        let BatchRecognitionRequest {
            profile,
            audio,
            authoritative_duration_millis,
            keyterms,
        } = request;
        let mut query = vec![
            ("model", MODEL),
            ("language", LANGUAGE),
            ("utterances", "true"),
            ("paragraphs", "true"),
        ];
        query.extend(keyterms.iter().map(|term| ("keyterm", term.as_str())));

        let response = self
            .client
            .post(self.endpoint.clone())
            .query(&query)
            .header(AUTHORIZATION, self.authorization.clone())
            .header(CONTENT_TYPE, "audio/ogg")
            .body(audio)
            .send()
            .await
            .map_err(|_| unknown("DEEPGRAM_NETWORK_ERROR", None))?;
        let status = response.status();
        let header_request_id = request_id_from_headers(response.headers());
        let retry_after_millis = retry_after_millis(response.headers());
        let body_limit = if status.is_success() {
            MAX_SUCCESS_BODY_BYTES
        } else {
            MAX_ERROR_BODY_BYTES
        };
        let body = read_bounded_body(response, body_limit, header_request_id.clone()).await?;

        if !status.is_success() {
            return Err(status_failure(
                status,
                header_request_id,
                retry_after_millis,
            ));
        }

        let parsed = dto::parse_response(&body, header_request_id)
            .map_err(|failure| unknown(failure.code, reference(failure.provider_request_id)))?;
        let parsed = timeline::normalize(parsed, authoritative_duration_millis)?;
        Ok(project_result(
            profile,
            authoritative_duration_millis,
            parsed,
        ))
    }
}

impl fmt::Debug for DeepgramBatchRecognizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeepgramBatchRecognizer")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl BatchRecognizer for DeepgramBatchRecognizer {
    fn capabilities(&self) -> &'static BatchCapabilityDescriptor {
        &CAPABILITIES
    }

    fn recognize(
        &self,
        request: BatchRecognitionRequest,
    ) -> BoxFuture<'_, Result<BatchRecognitionResult, RecognitionFailure>> {
        Box::pin(self.recognize_inner(request))
    }
}

fn profile_matches(request: &BatchRecognitionRequest) -> bool {
    request.profile.contract_version() == CONTRACT_VERSION
        && request.profile.provider() == PROVIDER
        && request.profile.model() == MODEL
        && request.profile.language() == LANGUAGE
}

fn project_result(
    profile: voicetext_speech::domain::batch::BatchProfile,
    authoritative_duration_millis: u64,
    parsed: ParsedBatchResult,
) -> BatchRecognitionResult {
    BatchRecognitionResult {
        profile,
        text: parsed.text,
        duration_millis: authoritative_duration_millis,
        provider_duration_millis: Some(parsed.provider_duration_ms),
        segments: parsed
            .segments
            .into_iter()
            .map(|segment| BatchSegment {
                start_millis: segment.start_ms,
                end_millis: segment.end_ms,
                text: segment.text,
                confidence: segment.confidence,
                speaker: segment.speaker,
            })
            .collect(),
        readable_segments: Some(
            parsed
                .readable_segments
                .into_iter()
                .map(|segment| BatchReadableSegment {
                    start_millis: segment.start_ms,
                    end_millis: segment.end_ms,
                    text: segment.text,
                    source_segment_indices: segment.source_segment_indices,
                })
                .collect(),
        ),
        provider_reference: parsed
            .provider_request_id
            .map(|id| ProviderReference::operation(ProviderOperationKind::RequestId, id)),
    }
}

async fn read_bounded_body(
    mut response: Response,
    limit: usize,
    provider_request_id: Option<String>,
) -> Result<Vec<u8>, RecognitionFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(unknown(
            "DEEPGRAM_RESPONSE_TOO_LARGE",
            reference(provider_request_id),
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        unknown(
            "DEEPGRAM_RESPONSE_READ_ERROR",
            reference(provider_request_id.clone()),
        )
    })? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(unknown(
                "DEEPGRAM_RESPONSE_TOO_LARGE",
                reference(provider_request_id),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn status_failure(
    status: StatusCode,
    provider_request_id: Option<String>,
    retry_after_millis: Option<u64>,
) -> RecognitionFailure {
    let provider_reference = reference(provider_request_id);
    match status.as_u16() {
        400 | 401 | 402 | 403 | 404 | 413 | 415 | 422 => {
            known_not_accepted(false, "DEEPGRAM_REQUEST_REJECTED", provider_reference, None)
        }
        429 => known_not_accepted(
            true,
            "DEEPGRAM_RATE_LIMITED",
            provider_reference,
            retry_after_millis,
        ),
        _ => unknown("DEEPGRAM_OUTCOME_UNKNOWN", provider_reference),
    }
}

fn request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-dg-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(dto::safe_request_id)
}

fn retry_after_millis(headers: &HeaderMap) -> Option<u64> {
    let seconds = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(seconds.saturating_mul(1_000).min(MAX_RETRY_AFTER_MILLIS))
}

fn reference(request_id: Option<String>) -> Option<ProviderReference> {
    request_id.map(|id| ProviderReference::operation(ProviderOperationKind::RequestId, id))
}

fn known_not_accepted(
    retryable: bool,
    code: &str,
    provider_reference: Option<ProviderReference>,
    retry_after_millis: Option<u64>,
) -> RecognitionFailure {
    RecognitionFailure::KnownNotAccepted {
        retryable,
        code: code.to_owned(),
        provider_reference,
        retry_after_millis,
    }
}

fn unknown(code: &str, provider_reference: Option<ProviderReference>) -> RecognitionFailure {
    RecognitionFailure::UnknownAfterSend {
        code: code.to_owned(),
        provider_reference,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    use voicetext_speech::domain::batch::BatchProfile;

    use super::*;

    #[derive(Debug)]
    struct CapturedRequest {
        target: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    #[derive(Debug)]
    struct FakeResponse {
        status: &'static str,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
        declared_length: Option<usize>,
    }

    fn request(profile: BatchProfile) -> BatchRecognitionRequest {
        BatchRecognitionRequest {
            profile,
            audio: b"OggS-synthetic".to_vec(),
            authoritative_duration_millis: 2_500,
            keyterms: vec!["Craig".into(), "release train".into()],
        }
    }

    fn deepgram_profile() -> BatchProfile {
        BatchProfile::new(2, PROVIDER, MODEL, LANGUAGE).unwrap()
    }

    fn spawn_fake(response: FakeResponse) -> (Url, Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let captured = read_request(&mut stream);
            let _receiver_may_be_unused = sender.send(captured);
            let declared_length = response.declared_length.unwrap_or(response.body.len());
            let mut head = format!(
                "HTTP/1.1 {}\r\nContent-Length: {declared_length}\r\nConnection: close\r\n",
                response.status
            );
            for (name, value) in response.headers {
                head.push_str(name);
                head.push_str(": ");
                head.push_str(&value);
                head.push_str("\r\n");
            }
            head.push_str("\r\n");
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&response.body).unwrap();
        });
        (
            Url::parse(&format!("http://{address}/v1/listen")).unwrap(),
            receiver,
        )
    }

    fn read_request(stream: &mut impl Read) -> CapturedRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 2_048];
        let header_end = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "request ended before headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let mut lines = head.split("\r\n");
        let target = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_owned();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect::<BTreeMap<_, _>>();
        let content_length = headers
            .get("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        while bytes.len() - header_end < content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "request ended before body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        CapturedRequest {
            target,
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    fn success_body() -> Vec<u8> {
        serde_json::json!({
            "metadata": {"duration": 2.75, "request_id": "metadata-request"},
            "results": {
                "channels": [{"alternatives": [{
                    "transcript": "release train",
                    "paragraphs": {"paragraphs": [{"sentences": [{
                        "text": "release train", "start": 0.0, "end": 2.5
                    }]}]}
                }]}],
                "utterances": [{
                    "start": 0.0, "end": 2.5, "transcript": "release train",
                    "confidence": 0.95, "speaker": 1
                }]
            }
        })
        .to_string()
        .into_bytes()
    }

    #[tokio::test]
    async fn sends_exact_nova_three_request_and_projects_the_response() {
        let (endpoint, captured) = spawn_fake(FakeResponse {
            status: "200 OK",
            headers: vec![("x-dg-request-id", "header-request".into())],
            body: success_body(),
            declared_length: None,
        });
        let recognizer =
            DeepgramBatchRecognizer::new(Client::new(), "test-secret", endpoint).unwrap();

        let result = recognizer
            .recognize(request(deepgram_profile()))
            .await
            .unwrap();
        let captured = captured.recv_timeout(Duration::from_secs(1)).unwrap();
        let target = Url::parse(&format!("http://localhost{}", captured.target)).unwrap();
        let query = target.query_pairs().collect::<Vec<_>>();

        assert!(query.contains(&("model".into(), "nova-3".into())));
        assert!(query.contains(&("language".into(), "multi".into())));
        assert!(query.contains(&("utterances".into(), "true".into())));
        assert!(query.contains(&("paragraphs".into(), "true".into())));
        assert_eq!(
            query
                .iter()
                .filter(|(name, _)| name == "keyterm")
                .map(|(_, value)| value.as_ref())
                .collect::<Vec<_>>(),
            vec!["Craig", "release train"]
        );
        assert_eq!(captured.headers["authorization"], "Token test-secret");
        assert_eq!(captured.headers["content-type"], "audio/ogg");
        assert_eq!(captured.body, b"OggS-synthetic");
        assert_eq!(result.profile, deepgram_profile());
        assert_eq!(result.duration_millis, 2_500);
        assert_eq!(result.provider_duration_millis, Some(2_750));
        assert_eq!(result.segments[0].speaker.as_deref(), Some("1"));
        let reference = result.provider_reference.unwrap();
        assert_eq!(reference.as_str(), "metadata-request");
        assert_eq!(
            reference.provider_operation().unwrap().kind(),
            ProviderOperationKind::RequestId
        );
    }

    #[tokio::test]
    async fn classifies_rate_limit_as_the_only_retryable_status() {
        let (endpoint, _) = spawn_fake(FakeResponse {
            status: "429 Too Many Requests",
            headers: vec![
                ("x-dg-request-id", "request-429".into()),
                ("Retry-After", "37".into()),
            ],
            body: b"{}".to_vec(),
            declared_length: None,
        });
        let recognizer = DeepgramBatchRecognizer::new(Client::new(), "test-key", endpoint).unwrap();

        let failure = recognizer
            .recognize(request(deepgram_profile()))
            .await
            .unwrap_err();

        assert_eq!(failure.code(), "DEEPGRAM_RATE_LIMITED");
        assert_eq!(
            failure.provider_reference().unwrap().as_str(),
            "request-429"
        );
        assert!(matches!(
            failure,
            RecognitionFailure::KnownNotAccepted {
                retryable: true,
                retry_after_millis: Some(30_000),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn fails_closed_for_unknown_status_and_oversized_error_body() {
        let (server_error_endpoint, _) = spawn_fake(FakeResponse {
            status: "500 Internal Server Error",
            headers: Vec::new(),
            body: b"{}".to_vec(),
            declared_length: None,
        });
        let recognizer =
            DeepgramBatchRecognizer::new(Client::new(), "test-key", server_error_endpoint).unwrap();
        assert!(matches!(
            recognizer.recognize(request(deepgram_profile())).await,
            Err(RecognitionFailure::UnknownAfterSend { ref code, .. })
                if code == "DEEPGRAM_OUTCOME_UNKNOWN"
        ));

        let (oversized_endpoint, _) = spawn_fake(FakeResponse {
            status: "400 Bad Request",
            headers: vec![("x-dg-request-id", "oversized".into())],
            body: Vec::new(),
            declared_length: Some(MAX_ERROR_BODY_BYTES + 1),
        });
        let recognizer =
            DeepgramBatchRecognizer::new(Client::new(), "test-key", oversized_endpoint).unwrap();
        let failure = recognizer
            .recognize(request(deepgram_profile()))
            .await
            .unwrap_err();
        assert_eq!(failure.code(), "DEEPGRAM_RESPONSE_TOO_LARGE");
        assert_eq!(failure.provider_reference().unwrap().as_str(), "oversized");
    }

    #[tokio::test]
    async fn rejects_profile_mismatch_before_provider_egress_and_redacts_the_key() {
        let endpoint = Url::parse("http://127.0.0.1:9/v1/listen").unwrap();
        let recognizer =
            DeepgramBatchRecognizer::new(Client::new(), "never-print-this", endpoint).unwrap();
        let wrong_profile = BatchProfile::new(3, PROVIDER, MODEL, LANGUAGE).unwrap();

        let failure = recognizer
            .recognize(request(wrong_profile))
            .await
            .unwrap_err();

        assert!(matches!(
            failure,
            RecognitionFailure::KnownNotAccepted {
                retryable: false,
                ref code,
                ..
            } if code == "BATCH_PROFILE_MISMATCH"
        ));
        assert!(!format!("{recognizer:?}").contains("never-print-this"));
    }
}
