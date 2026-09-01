use std::collections::HashSet;
use std::fmt;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::RETRY_AFTER;
use tokio_tungstenite::tungstenite::http::{HeaderMap, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use url::Url;
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
        let (socket, response) =
            match connect_async_with_config(handshake, Some(config), false).await {
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
            let mut writer = self.writer.lock().await;
            let mut state = self.state.lock().await;
            writer
                .send(Message::text(payload))
                .await
                .map_err(|_| unknown("ELEVENLABS_LIVE_AUDIO_WRITE_FAILED", None))?;
            state.record_audio(frame.pcm_s16le.len());
            Ok(())
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
            let mut writer = self.writer.lock().await;
            let mut state = self.state.lock().await;
            if !state.has_user_audio() {
                return Ok(());
            }
            state.begin_finalize();
            let result = writer
                .send(Message::text(payload))
                .await
                .map_err(|_| unknown("ELEVENLABS_LIVE_FINALIZE_WRITE_FAILED", None));
            if result.is_err() {
                state.cancel_finalize();
            }
            result
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            self.writer
                .lock()
                .await
                .close()
                .await
                .map_err(|_| unknown("ELEVENLABS_LIVE_CLOSE_FAILED", None))
        })
    }
}

fn validate_profile(request: &LiveRecognitionRequest) -> Result<(), RecognitionFailure> {
    let valid_rate = matches!(request.sample_rate_hz, 16_000 | 48_000);
    if request.profile.protocol_version != PROTOCOL_VERSION
        || request.profile.provider != PROVIDER
        || request.profile.model != MODEL
        || !live_dto::valid_language_code(&request.profile.language)
        || request.channels != CHANNELS
        || !valid_rate
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

/// Adapts optional generic keyterm hints to the stricter `ElevenLabs` realtime limits.
///
/// Unsupported hints are omitted instead of rejecting recognition: keyterms improve quality but
/// are not part of the transcript's correctness contract.
fn canonicalize_keyterms(input: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut output = Vec::new();
    for term in input {
        let normalized = term.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() || normalized.chars().count() > MAX_KEYTERM_CHARS {
            continue;
        }
        if seen.insert(normalized.clone()) {
            output.push(normalized);
            if output.len() == MAX_KEYTERMS {
                break;
            }
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
mod tests {
    use base64::Engine;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use voicetext_speech::application::ports::{LiveProfile, LiveTranscriptStability};
    use voicetext_speech::domain::live::LiveSession;

    use super::*;

    fn recognition_request(sample_rate_hz: u32, language: &str) -> LiveRecognitionRequest {
        LiveRecognitionRequest {
            profile: LiveProfile {
                protocol_version: PROTOCOL_VERSION,
                provider: PROVIDER.into(),
                model: MODEL.into(),
                language: language.into(),
            },
            sample_rate_hz,
            channels: CHANNELS,
            keyterms: vec![" Craig ".into(), "Craig".into(), "release train".into()],
        }
    }

    fn audio_frame() -> LiveAudioFrame {
        let mut lifecycle = LiveSession::new();
        lifecycle.mark_ready().unwrap();
        LiveAudioFrame {
            sequence: lifecycle.accept_audio().unwrap(),
            pcm_s16le: vec![1, 2, 3, 4],
        }
    }

    #[allow(clippy::result_large_err)] // Imposed by tungstenite's test handshake callback.
    async fn spawn_success() -> (
        Url,
        tokio::task::JoinHandle<(String, String, Vec<u8>, Vec<u8>)>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (sender, receiver) = oneshot::channel();
            let callback = move |request: &Request, response: Response| {
                sender
                    .send((
                        request.uri().to_string(),
                        request.headers()["xi-api-key"].to_str().unwrap().to_owned(),
                    ))
                    .unwrap();
                Ok(response)
            };
            let mut socket = accept_hdr_async(stream, callback).await.unwrap();
            let (target, api_key) = receiver.await.unwrap();
            let (audio, audio_commit) = decode_audio(socket.next().await.unwrap().unwrap());
            let (commit_audio, commit) = decode_audio(socket.next().await.unwrap().unwrap());
            assert!(!audio_commit);
            assert!(commit);
            for payload in [
                r#"{"message_type":"session_started","session_id":"session-1"}"#,
                r#"{"message_type":"partial_transcript","text":"hel"}"#,
                r#"{"message_type":"final_transcript","text":"hello"}"#,
                r#"{"message_type":"final_transcript_with_timestamps","text":"HELLO","words":[{"end":0.0}]}"#,
                r#"{"message_type":"committed_transcript","text":"hello tail"}"#,
                r#"{"message_type":"committed_transcript_with_timestamps","text":"HELLO TAIL","words":[{"end":0.0}]}"#,
                r#"{"message_type":"rate_limited"}"#,
            ] {
                socket.send(Message::text(payload)).await.unwrap();
            }
            let _ = socket.next().await;
            (target, api_key, audio, commit_audio)
        });
        (
            Url::parse(&format!(
                "ws://{address}/v1/speech-to-text/realtime?discard=true"
            ))
            .unwrap(),
            handle,
        )
    }

    fn decode_audio(message: Message) -> (Vec<u8>, bool) {
        let text = message.into_text().unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["message_type"], "input_audio_chunk");
        assert_eq!(value["sample_rate"], 16_000);
        (
            base64::engine::general_purpose::STANDARD
                .decode(value["audio_base_64"].as_str().unwrap())
                .unwrap(),
            value["commit"].as_bool().unwrap(),
        )
    }

    #[test]
    fn adapts_optional_keyterms_to_realtime_provider_limits() {
        let mut input = vec![
            " Craig ".into(),
            "live Pipecat assistant".into(),
            "   ".into(),
            "Craig".into(),
            "release   train".into(),
        ];
        input.extend((0..60).map(|index| format!("term-{index:02}")));

        let keyterms = canonicalize_keyterms(&input);

        assert_eq!(keyterms.len(), MAX_KEYTERMS);
        assert_eq!(keyterms[0], "Craig");
        assert_eq!(keyterms[1], "release train");
        assert_eq!(keyterms[2], "term-00");
        assert_eq!(keyterms[MAX_KEYTERMS - 1], "term-47");
        assert!(!keyterms.iter().any(|term| term == "live Pipecat assistant"));
    }

    #[tokio::test]
    async fn supports_legacy_16k_pcm_vad_segments_terminal_flush_and_deduplicated_events() {
        let (endpoint, server) = spawn_success().await;
        let factory = ElevenLabsLiveRecognizer::new("test-secret", endpoint).unwrap();
        let session = factory
            .open(recognition_request(16_000, "ru"))
            .await
            .unwrap();
        session.write_audio(audio_frame()).await.unwrap();
        session.finalize().await.unwrap();

        for stability in [
            LiveTranscriptStability::Partial,
            LiveTranscriptStability::SegmentFinal,
            LiveTranscriptStability::UtteranceFinal,
        ] {
            let LiveRecognitionEvent::Transcript(transcript) =
                session.next_event().await.unwrap().unwrap()
            else {
                panic!("expected transcript");
            };
            assert_eq!(transcript.stability, stability);
        }
        assert_eq!(
            session.next_event().await.unwrap(),
            Some(LiveRecognitionEvent::FinalizeResultObserved)
        );
        let error = session.next_event().await.unwrap_err();
        assert!(matches!(
            error,
            RecognitionFailure::KnownAcceptedTerminal { .. }
        ));
        assert_eq!(error.provider_reference().unwrap().as_str(), "session-1");
        session.close().await.unwrap();
        let (target, key, audio, commit_audio) = server.await.unwrap();
        assert_eq!(key, "test-secret");
        assert_eq!(audio, vec![1, 2, 3, 4]);
        assert!(commit_audio.is_empty());
        let url = Url::parse(&format!("ws://localhost{target}")).unwrap();
        assert!(
            url.query_pairs()
                .any(|pair| pair == ("audio_format".into(), "pcm_16000".into()))
        );
        assert!(
            url.query_pairs()
                .any(|pair| pair == ("language_code".into(), "ru".into()))
        );
        assert!(
            !url.query_pairs()
                .any(|(key, _)| key == "include_language_detection")
        );
        assert!(
            url.query_pairs()
                .any(|pair| pair == ("commit_strategy".into(), "vad".into()))
        );
        assert!(
            url.query_pairs()
                .any(|pair| pair == ("vad_silence_threshold_secs".into(), "1.5".into()))
        );
        assert_eq!(
            url.query_pairs()
                .filter(|(key, _)| key == "keyterms")
                .count(),
            2
        );
        assert!(!format!("{factory:?}").contains("test-secret"));
    }
}
