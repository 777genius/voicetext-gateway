use std::fmt;

use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, Response, StatusCode, Url};
use thiserror::Error;
use voicetext_speech::application::batch_capabilities::BatchCapabilityDescriptor;
use voicetext_speech::application::ports::{
    BatchRecognitionRequest, BatchRecognitionResult, BatchRecognizer, BatchSegment, BoxFuture,
    ProviderOperationKind, ProviderReference, RecognitionFailure,
};

use super::batch_capabilities::{self, CAPABILITIES};
use super::batch_keyterms::canonicalize_keyterms;
use super::dto::{self, ParsedBatchResult, ProviderIdentity};

pub(super) const CONTRACT_VERSION: u16 = 3;
pub(super) const PROVIDER: &str = "elevenlabs";
pub(super) const MODEL: &str = "scribe_v2";
pub(super) const LANGUAGE: &str = "multi";
pub(super) const MAX_AUDIO_BYTES: usize = 500 * 1_024 * 1_024;
pub(super) const MAX_KEYTERMS: usize = 100;
const MAX_SUCCESS_BODY_BYTES: usize = 16 * 1_024 * 1_024;
const MAX_ERROR_BODY_BYTES: usize = 8 * 1_024;
const MAX_RETRY_AFTER_MILLIS: u64 = 30_000;

/// Invalid injected configuration for an `ElevenLabs` batch adapter.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ElevenLabsConfigurationError {
    #[error("ElevenLabs API key must be non-empty and unpadded")]
    InvalidApiKey,
    #[error("ElevenLabs API key cannot be represented as an HTTP header")]
    InvalidApiKeyHeader,
}

pub struct ElevenLabsBatchRecognizer {
    client: Client,
    api_key: HeaderValue,
    endpoint: Url,
}

impl ElevenLabsBatchRecognizer {
    ///
    /// # Errors
    ///
    /// Returns [`ElevenLabsConfigurationError`] for an empty, padded, or invalid API key.
    pub fn new(
        client: Client,
        api_key: &str,
        endpoint: Url,
    ) -> Result<Self, ElevenLabsConfigurationError> {
        if api_key.is_empty() || api_key.trim() != api_key {
            return Err(ElevenLabsConfigurationError::InvalidApiKey);
        }
        let mut api_key = HeaderValue::from_str(api_key)
            .map_err(|_| ElevenLabsConfigurationError::InvalidApiKeyHeader)?;
        api_key.set_sensitive(true);
        Ok(Self {
            client,
            api_key,
            endpoint,
        })
    }

    async fn recognize_inner(
        &self,
        request: BatchRecognitionRequest,
    ) -> Result<BatchRecognitionResult, RecognitionFailure> {
        validate_request(&request)?;
        let BatchRecognitionRequest {
            profile,
            audio,
            authoritative_duration_millis,
            keyterms,
        } = request;
        let keyterms = canonicalize_keyterms(keyterms);
        let audio = Part::bytes(audio)
            .file_name("audio.ogg")
            .mime_str("audio/ogg")
            .map_err(|_| known_not_accepted(false, "ELEVENLABS_INVALID_AUDIO", None, None))?;
        let mut form = Form::new()
            .part("file", audio)
            .text("model_id", MODEL)
            .text("timestamps_granularity", "word")
            .text("diarize", "false")
            .text("tag_audio_events", "false")
            .text("use_multi_channel", "false")
            .text("webhook", "false")
            .text("file_format", "other");
        for keyterm in keyterms {
            form = form.text("keyterms", keyterm);
        }

        let response = self
            .client
            .post(self.endpoint.clone())
            .header("xi-api-key", self.api_key.clone())
            .multipart(form)
            .send()
            .await
            .map_err(|error| classify_send_error(&error))?;
        let status = response.status();
        let provider_request_id = request_id_from_headers(response.headers());
        let retry_after_millis = retry_after_millis(response.headers());

        if !status.is_success() {
            consume_bounded_error_body(response).await;
            return Err(status_failure(
                status,
                provider_request_id,
                retry_after_millis,
            ));
        }
        let body = read_bounded_success_body(response, provider_request_id.clone()).await?;
        let parsed = dto::parse_response(&body, authoritative_duration_millis, provider_request_id)
            .map_err(|failure| {
                unknown(failure.code, identity_reference(failure.provider_identity))
            })?;
        Ok(project_result(
            profile,
            authoritative_duration_millis,
            parsed,
        ))
    }
}

impl fmt::Debug for ElevenLabsBatchRecognizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ElevenLabsBatchRecognizer")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl BatchRecognizer for ElevenLabsBatchRecognizer {
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

fn validate_request(request: &BatchRecognitionRequest) -> Result<(), RecognitionFailure> {
    if request.profile.contract_version() != CONTRACT_VERSION
        || request.profile.provider() != PROVIDER
        || request.profile.model() != MODEL
        || request.profile.language() != LANGUAGE
    {
        return Err(known_not_accepted(
            false,
            "BATCH_PROFILE_MISMATCH",
            None,
            None,
        ));
    }
    if request.audio.is_empty() || request.audio.len() > MAX_AUDIO_BYTES {
        return Err(known_not_accepted(
            false,
            "ELEVENLABS_INVALID_AUDIO",
            None,
            None,
        ));
    }
    if request.authoritative_duration_millis == 0 {
        return Err(known_not_accepted(
            false,
            "ELEVENLABS_INVALID_DURATION",
            None,
            None,
        ));
    }
    if !batch_capabilities::matches(request) {
        return Err(known_not_accepted(
            false,
            "ELEVENLABS_REQUEST_CAPABILITY_MISMATCH",
            None,
            None,
        ));
    }
    Ok(())
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
        provider_duration_millis: parsed.provider_duration_millis,
        segments: parsed
            .segments
            .into_iter()
            .map(|segment| BatchSegment {
                start_millis: segment.start_millis,
                end_millis: segment.end_millis,
                text: segment.text,
                confidence: segment.confidence,
                speaker: None,
            })
            .collect(),
        readable_segments: None,
        provider_reference: identity_reference(parsed.provider_identity),
    }
}

fn classify_send_error(error: &reqwest::Error) -> RecognitionFailure {
    if error.is_connect() || error.is_builder() {
        known_not_accepted(true, "ELEVENLABS_BEFORE_SEND", None, None)
    } else {
        unknown("ELEVENLABS_NETWORK_ERROR", None)
    }
}

async fn read_bounded_success_body(
    mut response: Response,
    provider_request_id: Option<String>,
) -> Result<Vec<u8>, RecognitionFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SUCCESS_BODY_BYTES as u64)
    {
        return Err(unknown(
            "ELEVENLABS_RESPONSE_TOO_LARGE",
            reference(provider_request_id),
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        unknown(
            "ELEVENLABS_RESPONSE_READ_ERROR",
            reference(provider_request_id.clone()),
        )
    })? {
        if body.len().saturating_add(chunk.len()) > MAX_SUCCESS_BODY_BYTES {
            return Err(unknown(
                "ELEVENLABS_RESPONSE_TOO_LARGE",
                reference(provider_request_id),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn consume_bounded_error_body(mut response: Response) {
    let mut consumed = 0_usize;
    while consumed <= MAX_ERROR_BODY_BYTES {
        let Ok(Some(chunk)) = response.chunk().await else {
            break;
        };
        consumed = consumed.saturating_add(chunk.len());
    }
}

fn status_failure(
    status: StatusCode,
    provider_request_id: Option<String>,
    retry_after_millis: Option<u64>,
) -> RecognitionFailure {
    let provider_reference = reference(provider_request_id);
    match status.as_u16() {
        400..=499 if status != StatusCode::TOO_MANY_REQUESTS => known_not_accepted(
            false,
            "ELEVENLABS_REQUEST_REJECTED",
            provider_reference,
            None,
        ),
        429 => known_not_accepted(
            true,
            "ELEVENLABS_RATE_LIMITED",
            provider_reference,
            retry_after_millis,
        ),
        _ => unknown("ELEVENLABS_OUTCOME_UNKNOWN", provider_reference),
    }
}

fn request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get("request-id")
        .or_else(|| headers.get("x-request-id"))
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

fn identity_reference(identity: Option<ProviderIdentity>) -> Option<ProviderReference> {
    let (kind, id) = match identity? {
        ProviderIdentity::RequestId(id) => (ProviderOperationKind::RequestId, id),
        ProviderIdentity::TranscriptionId(id) => (ProviderOperationKind::TranscriptionId, id),
    };
    Some(ProviderReference::operation(kind, id))
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
    fn profile() -> BatchProfile {
        BatchProfile::new(CONTRACT_VERSION, PROVIDER, MODEL, LANGUAGE).unwrap()
    }
    fn request() -> BatchRecognitionRequest {
        BatchRecognitionRequest {
            profile: profile(),
            audio: b"OggS-synthetic".to_vec(),
            authoritative_duration_millis: 2_500,
            keyterms: vec!["Craig".into(), "release train".into()],
        }
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
            let _ = sender.send(read_request(&mut stream));
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
            if stream.write_all(head.as_bytes()).is_ok() {
                let _ = stream.write_all(&response.body);
            }
        });
        (
            Url::parse(&format!("http://{address}/v1/speech-to-text")).unwrap(),
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
        let headers = head
            .split("\r\n")
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect::<BTreeMap<_, _>>();
        let content_length = headers["content-length"].parse::<usize>().unwrap();
        while bytes.len() - header_end < content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "request ended before body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        CapturedRequest {
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }
    fn success_body() -> Vec<u8> {
        serde_json::json!({
            "language_code":"en", "language_probability":0.99,
            "audio_duration_secs":2.5, "transcription_id":"body-request",
            "text":"release train", "words":[
                {"type":"word","text":"release","start":0.0,"end":1.0,"logprob":-0.1},
                {"type":"spacing","text":" "},
                {"type":"word","text":"train","start":1.0,"end":2.5,"logprob":-0.2}
            ]
        })
        .to_string()
        .into_bytes()
    }
    #[tokio::test]
    async fn sends_exact_scribe_v2_multipart_and_projects_response() {
        let (endpoint, captured) = spawn_fake(FakeResponse {
            status: "200 OK",
            headers: vec![("request-id", "header-request".into())],
            body: success_body(),
            declared_length: None,
        });
        let recognizer =
            ElevenLabsBatchRecognizer::new(Client::new(), "test-secret", endpoint).unwrap();

        let result = recognizer.recognize(request()).await.unwrap();
        let captured = captured.recv_timeout(Duration::from_secs(1)).unwrap();
        let body = String::from_utf8_lossy(&captured.body);

        assert_eq!(captured.headers["xi-api-key"], "test-secret");
        assert!(body.contains("name=\"file\"; filename=\"audio.ogg\""));
        assert!(body.contains("Content-Type: audio/ogg"));
        assert!(body.contains("name=\"model_id\"\r\n\r\nscribe_v2"));
        for field in [
            "timestamps_granularity\"\r\n\r\nword",
            "diarize\"\r\n\r\nfalse",
            "tag_audio_events\"\r\n\r\nfalse",
            "use_multi_channel\"\r\n\r\nfalse",
            "webhook\"\r\n\r\nfalse",
            "file_format\"\r\n\r\nother",
        ] {
            assert!(body.contains(field), "missing multipart field {field}");
        }
        assert!(!body.contains("name=\"language_code\""));
        assert_eq!(body.matches("name=\"keyterms\"").count(), 2);
        assert!(result.segments[0].confidence.is_some());
        let operation = result
            .provider_reference
            .unwrap()
            .provider_operation()
            .unwrap();
        assert_eq!(operation.id(), "body-request");
        assert_eq!(operation.kind(), ProviderOperationKind::TranscriptionId);
    }
    #[tokio::test]
    async fn classifies_statuses_without_unbounded_error_reads() {
        for (status, code) in [
            ("418 I'm a teapot", "ELEVENLABS_REQUEST_REJECTED"),
            ("429 Too Many Requests", "ELEVENLABS_RATE_LIMITED"),
            ("500 Internal Server Error", "ELEVENLABS_OUTCOME_UNKNOWN"),
        ] {
            let (endpoint, _) = spawn_fake(FakeResponse {
                status,
                headers: vec![("x-request-id", "status-request".into())],
                body: vec![b'x'; MAX_ERROR_BODY_BYTES + 1],
                declared_length: None,
            });
            let recognizer =
                ElevenLabsBatchRecognizer::new(Client::new(), "test-key", endpoint).unwrap();
            let failure = recognizer.recognize(request()).await.unwrap_err();
            assert_eq!(failure.code(), code);
            assert_eq!(
                failure.provider_reference().unwrap().as_str(),
                "status-request"
            );
            if status.starts_with("429") {
                assert!(matches!(
                    failure,
                    RecognitionFailure::KnownNotAccepted {
                        retryable: true,
                        ..
                    }
                ));
            } else if status.starts_with('4') {
                assert!(matches!(
                    failure,
                    RecognitionFailure::KnownNotAccepted {
                        retryable: false,
                        ..
                    }
                ));
            } else {
                assert!(matches!(
                    failure,
                    RecognitionFailure::UnknownAfterSend { .. }
                ));
            }
        }
    }
    #[tokio::test]
    async fn rejects_malformed_and_oversized_success_bodies_after_send() {
        for (body, declared_length, code) in [
            (b"not-json".to_vec(), None, "ELEVENLABS_MALFORMED_RESPONSE"),
            (
                Vec::new(),
                Some(MAX_SUCCESS_BODY_BYTES + 1),
                "ELEVENLABS_RESPONSE_TOO_LARGE",
            ),
        ] {
            let (endpoint, _) = spawn_fake(FakeResponse {
                status: "200 OK",
                headers: Vec::new(),
                body,
                declared_length,
            });
            let recognizer =
                ElevenLabsBatchRecognizer::new(Client::new(), "test-key", endpoint).unwrap();
            let failure = recognizer.recognize(request()).await.unwrap_err();
            assert_eq!(failure.code(), code);
            assert!(matches!(
                failure,
                RecognitionFailure::UnknownAfterSend { .. }
            ));
        }
    }

    #[tokio::test]
    async fn rejects_invalid_inputs_before_egress_and_redacts_secret() {
        let endpoint = Url::parse("http://127.0.0.1:9/v1/speech-to-text").unwrap();
        let recognizer =
            ElevenLabsBatchRecognizer::new(Client::new(), "never-print-this", endpoint).unwrap();
        let mut invalid = request();
        invalid.profile = BatchProfile::new(2, PROVIDER, MODEL, LANGUAGE).unwrap();
        let failure = recognizer.recognize(invalid).await.unwrap_err();
        assert!(matches!(
            failure,
            RecognitionFailure::KnownNotAccepted { retryable: false, ref code, .. }
                if code == "BATCH_PROFILE_MISMATCH"
        ));
        assert!(!format!("{recognizer:?}").contains("never-print-this"));
        let mut excessive = request();
        excessive.keyterms = vec!["x".into(); MAX_KEYTERMS + 1];
        assert!(matches!(
            recognizer.recognize(excessive).await,
            Err(RecognitionFailure::KnownNotAccepted {
                retryable: false,
                ..
            })
        ));
        let mut punctuation = request();
        punctuation.keyterms = vec!["bad<tag".into()];
        assert!(matches!(
            recognizer.recognize(punctuation).await,
            Err(RecognitionFailure::KnownNotAccepted {
                retryable: false,
                ..
            })
        ));

        let failure = recognizer.recognize(request()).await.unwrap_err();
        assert!(matches!(
            failure,
            RecognitionFailure::KnownNotAccepted { retryable: true, ref code, .. }
                if code == "ELEVENLABS_BEFORE_SEND"
        ));
    }
}
