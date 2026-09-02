use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::RETRY_AFTER;
use tokio_tungstenite::tungstenite::http::{HeaderMap, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use url::Url;
use voicetext_speech::application::batch_capabilities::{TextLengthUnit, TimestampProvenance};
use voicetext_speech::application::live_capabilities::{
    LiveCapabilityDescriptor, LiveCapabilityRequest, LiveFinalizedCapability, LiveInputFormat,
    LiveLanguageHints, LiveProviderLimits, LiveTimestampCapability,
};
use voicetext_speech::application::ports::{
    BoxFuture, LiveAudioFrame, LiveRecognitionEvent, LiveRecognitionRequest, LiveRecognizerFactory,
    LiveRecognizerSession, ProviderReference, RecognitionFailure,
};

use super::live_dto::{self, ParsedLiveMessage};
use super::live_state::LiveState;

const PROTOCOL_VERSION: u16 = 2;
const PROVIDER: &str = "elevenlabs";
const MODEL: &str = "scribe_v2_realtime";
const CHANNELS: u8 = 1;
const MAX_AUDIO_FRAME_BYTES: usize = 1_024 * 1_024;
const MAX_KEYTERMS: usize = 50;
const MAX_KEYTERM_CHARS: usize = 20;
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
    timestamp_provenance: TimestampProvenance::GatewaySynthesizedFromAcceptedAudio,
    finalized_events: LiveFinalizedCapability::SegmentAndUtterance,
    language_hints: LiveLanguageHints::AsciiCode {
        maximum_bytes: 10,
        hyphen_at_edges: true,
    },
    diarization: false,
    key_terms: true,
    input_formats: INPUT_FORMATS,
    provider_limits: LiveProviderLimits {
        maximum_public_input_frame_bytes: 64 * 1_024,
        maximum_input_frame_bytes: MAX_AUDIO_FRAME_BYTES,
        maximum_key_terms: MAX_KEYTERMS,
        maximum_key_term_bytes: None,
        maximum_public_key_term_utf16_units: 256,
        maximum_key_term_characters: Some(MAX_KEYTERM_CHARS),
        key_term_character_unit: Some(TextLengthUnit::UnicodeScalars),
        maximum_public_key_term_total_utf16_units: 8_192,
        normalize_key_term_whitespace: true,
    },
};

type ClientSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ClientWriter = SplitSink<ClientSocket, Message>;
type ClientReader = SplitStream<ClientSocket>;

/// Invalid explicitly injected configuration for an `ElevenLabs` live adapter.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ElevenLabsLiveConfigurationError {
    #[error("ElevenLabs API key must be non-empty and unpadded")]
    InvalidApiKey,
    #[error("ElevenLabs API key cannot be represented as an HTTP header")]
    InvalidApiKeyHeader,
    #[error("ElevenLabs live endpoint must use ws or wss")]
    InvalidEndpoint,
}

/// `ElevenLabs` Scribe v2 Realtime implementation of the live factory port.
pub struct ElevenLabsLiveRecognizer {
    api_key: HeaderValue,
    endpoint: Url,
}

impl ElevenLabsLiveRecognizer {
    /// Creates a live factory from an explicitly injected API key and endpoint.
    /// # Errors
    /// Returns [`ElevenLabsLiveConfigurationError`] when configuration cannot produce a safe
    /// WebSocket request.
    pub fn new(api_key: &str, endpoint: Url) -> Result<Self, ElevenLabsLiveConfigurationError> {
        if api_key.is_empty() || api_key.trim() != api_key {
            return Err(ElevenLabsLiveConfigurationError::InvalidApiKey);
        }
        if !matches!(endpoint.scheme(), "ws" | "wss") || endpoint.host().is_none() {
            return Err(ElevenLabsLiveConfigurationError::InvalidEndpoint);
        }
        let mut api_key = HeaderValue::from_str(api_key)
            .map_err(|_| ElevenLabsLiveConfigurationError::InvalidApiKeyHeader)?;
        api_key.set_sensitive(true);
        Ok(Self { api_key, endpoint })
    }

    async fn open_inner(
        &self,
        request: LiveRecognitionRequest,
    ) -> Result<Box<dyn LiveRecognizerSession>, RecognitionFailure> {
        validate_profile(&request)?;
        let keyterms = canonicalize_keyterms(&request.keyterms);
        let url = live_dto::build_url(
            self.endpoint.clone(),
            request.sample_rate_hz,
            &request.profile.language,
            &keyterms,
        );
        let mut handshake = url.as_str().into_client_request().map_err(|_| {
            known_not_accepted(false, "ELEVENLABS_LIVE_REQUEST_INVALID", None, None)
        })?;
        handshake
            .headers_mut()
            .insert("xi-api-key", self.api_key.clone());
        let config = WebSocketConfig::default()
            .max_message_size(Some(live_dto::MAX_PROVIDER_MESSAGE_BYTES))
            .max_frame_size(Some(live_dto::MAX_PROVIDER_MESSAGE_BYTES));
        let connected = timeout(
            CONNECT_TIMEOUT,
            connect_async_with_config(handshake, Some(config), false),
        )
        .await
        .map_err(|_| known_not_accepted(true, "ELEVENLABS_LIVE_CONNECT_TIMEOUT", None, None))?;
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
                    "ELEVENLABS_LIVE_CONNECT_FAILED",
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
        Ok(Box::new(ElevenLabsLiveSession {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
            state: Mutex::new(LiveState::new(request.sample_rate_hz, provider_reference)),
            sample_rate_hz: request.sample_rate_hz,
        }))
    }
}

impl fmt::Debug for ElevenLabsLiveRecognizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ElevenLabsLiveRecognizer")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl LiveRecognizerFactory for ElevenLabsLiveRecognizer {
    fn capabilities(&self) -> &'static LiveCapabilityDescriptor {
        &CAPABILITIES
    }

    fn open(
        &self,
        request: LiveRecognitionRequest,
    ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>> {
        Box::pin(self.open_inner(request))
    }
}

pub struct ElevenLabsLiveSession {
    writer: Mutex<ClientWriter>,
    reader: Mutex<ClientReader>,
    state: Mutex<LiveState>,
    sample_rate_hz: u32,
}

impl ElevenLabsLiveSession {
    async fn next_event_inner(&self) -> Result<Option<LiveRecognitionEvent>, RecognitionFailure> {
        if let Some(event) = self.state.lock().await.pop_pending() {
            return Ok(Some(event));
        }
        let mut reader = self.reader.lock().await;
        let mut ignored = 0_usize;
        loop {
            let message = match reader.next().await {
                Some(Ok(message)) => message,
                Some(Err(WebSocketError::Capacity(_))) => {
                    return Err(self
                        .unknown_with_reference("ELEVENLABS_LIVE_RESPONSE_TOO_LARGE")
                        .await);
                }
                Some(Err(_)) | None => {
                    return Err(self
                        .unknown_with_reference("ELEVENLABS_LIVE_READ_FAILED")
                        .await);
                }
            };
            match message {
                Message::Text(payload) => {
                    let parsed =
                        live_dto::parse_text(payload.as_str()).map_err(|failure| failure.code);
                    let parsed = match parsed {
                        Ok(parsed) => parsed,
                        Err(code) => return Err(self.unknown_with_reference(code).await),
                    };
                    if let ParsedLiveMessage::TerminalError { code } = parsed {
                        return Err(self.terminal_with_reference(code).await);
                    }
                    let mut state = self.state.lock().await;
                    match state.apply(parsed) {
                        Ok(Some(event)) => return Ok(Some(event)),
                        Ok(None) => {}
                        Err(code) => {
                            return Err(unknown(code, state.provider_reference()));
                        }
                    }
                }
                Message::Ping(payload) => {
                    self.writer
                        .lock()
                        .await
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|_| unknown("ELEVENLABS_LIVE_PONG_FAILED", None))?;
                }
                Message::Pong(_) => {}
                Message::Close(_) => {
                    return Err(self.unknown_with_reference("ELEVENLABS_LIVE_CLOSED").await);
                }
                Message::Binary(_) | Message::Frame(_) => {
                    return Err(self
                        .unknown_with_reference("ELEVENLABS_LIVE_MALFORMED_RESPONSE")
                        .await);
                }
            }
            ignored += 1;
            if ignored == MAX_IGNORED_MESSAGES_BEFORE_YIELD {
                ignored = 0;
                tokio::task::yield_now().await;
            }
        }
    }

    async fn unknown_with_reference(&self, code: &str) -> RecognitionFailure {
        unknown(code, self.state.lock().await.provider_reference())
    }

    async fn terminal_with_reference(&self, code: &str) -> RecognitionFailure {
        known_terminal(code, self.state.lock().await.provider_reference())
    }
}

impl fmt::Debug for ElevenLabsLiveSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ElevenLabsLiveSession")
            .field("sample_rate_hz", &self.sample_rate_hz)
            .finish_non_exhaustive()
    }
}

impl LiveRecognizerSession for ElevenLabsLiveSession {
    fn write_audio(&self, frame: LiveAudioFrame) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            if frame.pcm_s16le.is_empty()
                || !frame.pcm_s16le.len().is_multiple_of(2)
                || frame.pcm_s16le.len() > MAX_AUDIO_FRAME_BYTES
            {
                return Err(known_not_accepted(
                    false,
                    "ELEVENLABS_LIVE_INVALID_AUDIO",
                    None,
                    None,
                ));
            }
            let payload = live_dto::encode_audio(&frame.pcm_s16le, self.sample_rate_hz, false);
            bounded_io(
                WRITE_TIMEOUT,
                async {
                    let mut writer = self.writer.lock().await;
                    let mut state = self.state.lock().await;
                    writer.send(Message::text(payload)).await?;
                    state.record_audio(frame.pcm_s16le.len());
                    Ok::<(), WebSocketError>(())
                },
                "ELEVENLABS_LIVE_AUDIO_WRITE_TIMEOUT",
                "ELEVENLABS_LIVE_AUDIO_WRITE_FAILED",
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
        Box::pin(async move {
            let payload = live_dto::encode_audio(&[], self.sample_rate_hz, true);
            bounded_io(
                WRITE_TIMEOUT,
                async {
                    let mut writer = self.writer.lock().await;
                    let mut state = self.state.lock().await;
                    if !state.has_user_audio() {
                        return Ok::<(), WebSocketError>(());
                    }
                    state.begin_finalize();
                    let result = writer.send(Message::text(payload)).await;
                    if result.is_err() {
                        state.cancel_finalize();
                    }
                    result
                },
                "ELEVENLABS_LIVE_FINALIZE_WRITE_TIMEOUT",
                "ELEVENLABS_LIVE_FINALIZE_WRITE_FAILED",
            )
            .await
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            bounded_io(
                CLOSE_TIMEOUT,
                async { self.writer.lock().await.close().await },
                "ELEVENLABS_LIVE_CLOSE_TIMEOUT",
                "ELEVENLABS_LIVE_CLOSE_FAILED",
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

fn validate_profile(request: &LiveRecognitionRequest) -> Result<(), RecognitionFailure> {
    let valid_rate = matches!(request.sample_rate_hz, 16_000 | 48_000);
    let input_format = match request.sample_rate_hz {
        16_000 => LiveInputFormat::PcmS16Le16KhzMono,
        48_000 => LiveInputFormat::PcmS16Le48KhzMono,
        _ => LiveInputFormat::Opus48KhzMono,
    };
    let terms = request
        .keyterms
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let capability_match = CAPABILITIES
        .validate(&LiveCapabilityRequest {
            timestamps: true,
            finalized_events: true,
            language_hint: Some(&request.profile.language),
            diarization: false,
            key_terms: &terms,
            input_format,
            input_frame_bytes: 2,
        })
        .is_ok();
    if request.profile.protocol_version != PROTOCOL_VERSION
        || request.profile.provider != PROVIDER
        || request.profile.model != MODEL
        || !live_dto::valid_language_code(&request.profile.language)
        || request.channels != CHANNELS
        || !valid_rate
        || !capability_match
    {
        return Err(known_not_accepted(
            false,
            "ELEVENLABS_LIVE_PROFILE_MISMATCH",
            None,
            None,
        ));
    }
    Ok(())
}

fn canonicalize_keyterms(input: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut output = Vec::new();
    for term in input {
        let normalized = term.split_whitespace().collect::<Vec<_>>().join(" ");
        if seen.insert(normalized.clone()) {
            output.push(normalized);
        }
    }
    output
}

fn handshake_status_failure(status: StatusCode, headers: &HeaderMap) -> RecognitionFailure {
    let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
    let code = if status == StatusCode::TOO_MANY_REQUESTS {
        "ELEVENLABS_LIVE_RATE_LIMITED"
    } else {
        "ELEVENLABS_LIVE_HANDSHAKE_REJECTED"
    };
    known_not_accepted(
        retryable,
        code,
        request_id_from_headers(headers).map(ProviderReference::new),
        retryable.then(|| retry_after_millis(headers)).flatten(),
    )
}

fn request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get("request-id")
        .or_else(|| headers.get("x-request-id"))
        .and_then(|value| value.to_str().ok())
        .and_then(live_dto::safe_reference)
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
