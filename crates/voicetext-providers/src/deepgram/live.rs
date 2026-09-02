use super::live_dto::{self, ParsedLiveMessage};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, RETRY_AFTER};
use tokio_tungstenite::tungstenite::http::{HeaderMap, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use url::Url;
use voicetext_speech::application::batch_capabilities::TimestampProvenance;
use voicetext_speech::application::live_capabilities::{
    LiveCapabilityDescriptor, LiveCapabilityRequest, LiveFinalizedCapability, LiveInputFormat,
    LiveLanguageHints, LiveProviderLimits, LiveTimestampCapability,
};
use voicetext_speech::application::ports::{
    BoxFuture, LiveAudioFrame, LiveRecognitionEvent, LiveRecognitionRequest, LiveRecognizerFactory,
    LiveRecognizerSession, ProviderReference, RecognitionFailure,
};

const PROTOCOL_VERSION: u16 = 2;
const PROVIDER: &str = "deepgram";
const MODEL: &str = "nova-3";
const CHANNELS: u8 = 1;
const MAX_IGNORED_MESSAGES_BEFORE_YIELD: usize = 32;
const MAX_RETRY_AFTER_MILLIS: u64 = 30_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const INPUT_FORMATS: &[LiveInputFormat] = &[
    LiveInputFormat::Opus48KhzMono,
    LiveInputFormat::PcmS16Le16KhzMono,
    LiveInputFormat::PcmS16Le48KhzMono,
];
const CAPABILITIES: LiveCapabilityDescriptor = LiveCapabilityDescriptor {
    protocol_version: PROTOCOL_VERSION,
    provider: PROVIDER,
    model: MODEL,
    timestamps: LiveTimestampCapability::Segment,
    timestamp_provenance: TimestampProvenance::ProviderNative,
    finalized_events: LiveFinalizedCapability::SegmentAndUtterance,
    language_hints: LiveLanguageHints::AsciiCode {
        maximum_bytes: 10,
        hyphen_at_edges: false,
    },
    diarization: false,
    key_terms: true,
    input_formats: INPUT_FORMATS,
    provider_limits: LiveProviderLimits {
        maximum_public_input_frame_bytes: 64 * 1_024,
        maximum_input_frame_bytes: 64 * 1_024,
        maximum_key_terms: 100,
        maximum_key_term_bytes: Some(256),
        maximum_public_key_term_utf16_units: 256,
        maximum_key_term_characters: None,
        key_term_character_unit: None,
        maximum_public_key_term_total_utf16_units: 8_192,
        normalize_key_term_whitespace: false,
    },
};

type ClientSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ClientWriter = SplitSink<ClientSocket, Message>;
type ClientReader = SplitStream<ClientSocket>;
/// Invalid explicitly injected configuration for a Deepgram live adapter.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DeepgramLiveConfigurationError {
    #[error("Deepgram API key must be non-empty and unpadded")]
    InvalidApiKey,
    #[error("Deepgram API key cannot be represented as an authorization header")]
    InvalidAuthorizationHeader,
    #[error("Deepgram live endpoint must use ws or wss")]
    InvalidEndpoint,
}

/// Deepgram Nova-3 implementation of the provider-neutral live factory port.
pub struct DeepgramLiveRecognizer {
    authorization: HeaderValue,
    endpoint: Url,
}
impl DeepgramLiveRecognizer {
    /// Creates a live factory from an explicitly injected API key and endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`DeepgramLiveConfigurationError`] when configuration cannot produce a safe
    /// WebSocket request.
    pub fn new(api_key: &str, endpoint: Url) -> Result<Self, DeepgramLiveConfigurationError> {
        if api_key.is_empty() || api_key.trim() != api_key {
            return Err(DeepgramLiveConfigurationError::InvalidApiKey);
        }
        if !matches!(endpoint.scheme(), "ws" | "wss") || endpoint.host().is_none() {
            return Err(DeepgramLiveConfigurationError::InvalidEndpoint);
        }
        let mut authorization = HeaderValue::from_str(&format!("Token {api_key}"))
            .map_err(|_| DeepgramLiveConfigurationError::InvalidAuthorizationHeader)?;
        authorization.set_sensitive(true);
        Ok(Self {
            authorization,
            endpoint,
        })
    }

    async fn open_inner(
        &self,
        request: LiveRecognitionRequest,
    ) -> Result<Box<dyn LiveRecognizerSession>, RecognitionFailure> {
        if !profile_matches(&request) || !capabilities_match(&request) {
            return Err(known_not_accepted(
                false,
                "DEEPGRAM_LIVE_PROFILE_MISMATCH",
                None,
                None,
            ));
        }
        let url = live_dto::build_url(
            self.endpoint.clone(),
            request.sample_rate_hz,
            &request.profile.language,
            &request.keyterms,
        );
        let mut handshake = url
            .as_str()
            .into_client_request()
            .map_err(|_| known_not_accepted(false, "DEEPGRAM_LIVE_REQUEST_INVALID", None, None))?;
        handshake
            .headers_mut()
            .insert(AUTHORIZATION, self.authorization.clone());
        let config = WebSocketConfig::default()
            .max_message_size(Some(live_dto::MAX_PROVIDER_TEXT_BYTES))
            .max_frame_size(Some(live_dto::MAX_PROVIDER_TEXT_BYTES));
        let connected = timeout(
            CONNECT_TIMEOUT,
            connect_async_with_config(handshake, Some(config), false),
        )
        .await
        .map_err(|_| known_not_accepted(true, "DEEPGRAM_LIVE_CONNECT_TIMEOUT", None, None))?;
        let (socket, response) = match connected {
            Ok(connected) => connected,
            Err(WebSocketError::Http(response)) => {
                return Err(handshake_status_failure(
                    response.status(),
                    response.headers(),
                ));
            }
            Err(_) => {
                return Err(known_not_accepted(
                    true,
                    "DEEPGRAM_LIVE_CONNECT_FAILED",
                    None,
                    None,
                ));
            }
        };
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(handshake_status_failure(
                response.status(),
                response.headers(),
            ));
        }

        let provider_reference =
            request_id_from_headers(response.headers()).map(ProviderReference::new);
        let (writer, reader) = socket.split();
        Ok(Box::new(DeepgramLiveSession {
            writer: Mutex::new(writer),
            reader: Mutex::new(ReadState {
                reader,
                pending: VecDeque::with_capacity(1),
                provider_reference,
            }),
        }))
    }
}

impl fmt::Debug for DeepgramLiveRecognizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeepgramLiveRecognizer")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl LiveRecognizerFactory for DeepgramLiveRecognizer {
    fn capabilities(
        &self,
    ) -> &'static voicetext_speech::application::live_capabilities::LiveCapabilityDescriptor {
        &CAPABILITIES
    }

    fn open(
        &self,
        request: LiveRecognitionRequest,
    ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>> {
        Box::pin(self.open_inner(request))
    }
}

fn capabilities_match(request: &LiveRecognitionRequest) -> bool {
    let terms = request
        .keyterms
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let input_format = match request.sample_rate_hz {
        16_000 => LiveInputFormat::PcmS16Le16KhzMono,
        48_000 => LiveInputFormat::PcmS16Le48KhzMono,
        _ => return false,
    };
    CAPABILITIES
        .validate(&LiveCapabilityRequest {
            timestamps: true,
            finalized_events: true,
            language_hint: Some(&request.profile.language),
            diarization: false,
            key_terms: &terms,
            input_format,
            input_frame_bytes: 2,
        })
        .is_ok()
}

/// One connected Deepgram live session with independent read and write locks.
struct DeepgramLiveSession {
    writer: Mutex<ClientWriter>,
    reader: Mutex<ReadState>,
}

struct ReadState {
    reader: ClientReader,
    pending: VecDeque<LiveRecognitionEvent>,
    provider_reference: Option<ProviderReference>,
}

impl DeepgramLiveSession {
    async fn write_message(
        &self,
        message: Message,
        timeout_code: &str,
        failure_code: &str,
    ) -> Result<(), RecognitionFailure> {
        bounded_io(
            WRITE_TIMEOUT,
            async { self.writer.lock().await.send(message).await },
            timeout_code,
            failure_code,
        )
        .await
    }

    async fn next_event_inner(&self) -> Result<Option<LiveRecognitionEvent>, RecognitionFailure> {
        let mut state = self.reader.lock().await;
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some(event));
        }
        let mut ignored = 0_usize;
        loop {
            let message = match state.reader.next().await {
                Some(Ok(message)) => message,
                Some(Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed))
                | None => return Ok(None),
                Some(Err(WebSocketError::Capacity(_))) => {
                    return Err(unknown(
                        "DEEPGRAM_LIVE_RESPONSE_TOO_LARGE",
                        state.provider_reference.clone(),
                    ));
                }
                Some(Err(_)) => {
                    return Err(unknown(
                        "DEEPGRAM_LIVE_READ_FAILED",
                        state.provider_reference.clone(),
                    ));
                }
            };
            match message {
                Message::Text(payload) => match live_dto::parse_text(payload.as_str()) {
                    Ok(ParsedLiveMessage::Events { primary, follow_up }) => {
                        if let Some(event) = follow_up {
                            state.pending.push_back(event);
                        }
                        return Ok(Some(primary));
                    }
                    Ok(ParsedLiveMessage::Metadata(request_id)) => {
                        if let Some(request_id) = request_id {
                            state.provider_reference = Some(ProviderReference::new(request_id));
                        }
                    }
                    Ok(ParsedLiveMessage::TerminalError) => {
                        return Err(known_terminal(
                            "DEEPGRAM_LIVE_PROVIDER_ERROR",
                            state.provider_reference.clone(),
                        ));
                    }
                    Ok(ParsedLiveMessage::Ignore) => {}
                    Err(failure) => {
                        return Err(unknown(failure.code, state.provider_reference.clone()));
                    }
                },
                Message::Close(_) => return Ok(None),
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Binary(_) | Message::Frame(_) => {
                    return Err(unknown(
                        "DEEPGRAM_LIVE_MALFORMED_RESPONSE",
                        state.provider_reference.clone(),
                    ));
                }
            }
            ignored += 1;
            if ignored == MAX_IGNORED_MESSAGES_BEFORE_YIELD {
                ignored = 0;
                tokio::task::yield_now().await;
            }
        }
    }
}

impl fmt::Debug for DeepgramLiveSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeepgramLiveSession")
            .finish_non_exhaustive()
    }
}

impl LiveRecognizerSession for DeepgramLiveSession {
    fn write_audio(&self, frame: LiveAudioFrame) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            self.write_message(
                Message::Binary(frame.pcm_s16le.into()),
                "DEEPGRAM_LIVE_AUDIO_WRITE_TIMEOUT",
                "DEEPGRAM_LIVE_AUDIO_WRITE_FAILED",
            )
            .await
        })
    }

    fn next_event(
        &self,
    ) -> BoxFuture<'_, Result<Option<LiveRecognitionEvent>, RecognitionFailure>> {
        Box::pin(self.next_event_inner())
    }

    fn finalize(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(self.write_message(
            Message::text(r#"{"type":"Finalize"}"#),
            "DEEPGRAM_LIVE_FINALIZE_WRITE_TIMEOUT",
            "DEEPGRAM_LIVE_FINALIZE_WRITE_FAILED",
        ))
    }

    fn close(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            bounded_io(
                CLOSE_TIMEOUT,
                async {
                    let mut writer = self.writer.lock().await;
                    let control = writer
                        .send(Message::text(r#"{"type":"CloseStream"}"#))
                        .await;
                    let closed = writer.close().await;
                    control.and(closed)
                },
                "DEEPGRAM_LIVE_CLOSE_TIMEOUT",
                "DEEPGRAM_LIVE_CLOSE_FAILED",
            )
            .await
        })
    }
}

async fn bounded_io<T>(
    maximum: Duration,
    operation: impl Future<Output = Result<T, WebSocketError>>,
    timeout_code: &str,
    failure_code: &str,
) -> Result<T, RecognitionFailure> {
    timeout(maximum, operation)
        .await
        .map_err(|_| unknown(timeout_code, None))?
        .map_err(|_| unknown(failure_code, None))
}

fn profile_matches(request: &LiveRecognitionRequest) -> bool {
    request.profile.protocol_version == PROTOCOL_VERSION
        && request.profile.provider == PROVIDER
        && request.profile.model == MODEL
        && live_dto::language_is_safe(&request.profile.language)
        && matches!(request.sample_rate_hz, 16_000 | 48_000)
        && request.channels == CHANNELS
}

fn handshake_status_failure(status: StatusCode, headers: &HeaderMap) -> RecognitionFailure {
    let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
    let code = if status == StatusCode::TOO_MANY_REQUESTS {
        "DEEPGRAM_LIVE_RATE_LIMITED"
    } else {
        "DEEPGRAM_LIVE_HANDSHAKE_REJECTED"
    };
    known_not_accepted(
        retryable,
        code,
        request_id_from_headers(headers).map(ProviderReference::new),
        retryable.then(|| retry_after_millis(headers)).flatten(),
    )
}

fn request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    ["x-dg-request-id", "dg-request-id"]
        .into_iter()
        .find_map(|name| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .and_then(super::dto::safe_request_id)
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

fn known_terminal(code: &str, provider_reference: Option<ProviderReference>) -> RecognitionFailure {
    RecognitionFailure::KnownAcceptedTerminal {
        code: code.to_owned(),
        provider_reference,
    }
}

fn unknown(code: &str, provider_reference: Option<ProviderReference>) -> RecognitionFailure {
    RecognitionFailure::UnknownAfterSend {
        code: code.to_owned(),
        provider_reference,
    }
}

#[cfg(test)]
mod tests;
