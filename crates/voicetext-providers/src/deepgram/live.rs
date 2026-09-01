use super::live_dto::{self, ParsedLiveMessage};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::fmt;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, RETRY_AFTER};
use tokio_tungstenite::tungstenite::http::{HeaderMap, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use url::Url;
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
        if !profile_matches(&request) {
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
    fn open(
        &self,
        request: LiveRecognitionRequest,
    ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>> {
        Box::pin(self.open_inner(request))
    }
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
    async fn write_message(&self, message: Message, code: &str) -> Result<(), RecognitionFailure> {
        self.writer
            .lock()
            .await
            .send(message)
            .await
            .map_err(|_| unknown(code, None))
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
            "DEEPGRAM_LIVE_FINALIZE_WRITE_FAILED",
        ))
    }

    fn close(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            let mut writer = self.writer.lock().await;
            let control = writer
                .send(Message::text(r#"{"type":"CloseStream"}"#))
                .await;
            let closed = writer.close().await;
            control
                .and(closed)
                .map_err(|_| unknown("DEEPGRAM_LIVE_CLOSE_FAILED", None))
        })
    }
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
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use voicetext_speech::application::ports::{
        LiveProfile, LiveTranscriptStability, RecognitionFailureClass,
    };
    use voicetext_speech::domain::live::LiveSession;

    use super::*;

    #[derive(Debug)]
    struct CapturedFlow {
        target: String,
        authorization: String,
        audio: Vec<u8>,
        finalize: String,
        close_stream: String,
        saw_close: bool,
    }
    fn recognition_request() -> LiveRecognitionRequest {
        LiveRecognitionRequest {
            profile: LiveProfile {
                protocol_version: 2,
                provider: PROVIDER.into(),
                model: MODEL.into(),
                language: "multi".into(),
            },
            sample_rate_hz: 48_000,
            channels: CHANNELS,
            keyterms: vec![" Craig ".into(), "release train".into()],
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
    #[test]
    fn accepts_only_supported_pcm_rates_and_projects_legacy_rate() {
        let mut request = recognition_request();
        request.sample_rate_hz = 16_000;
        assert!(profile_matches(&request));
        let endpoint = Url::parse("wss://api.deepgram.com/v1/listen").unwrap();
        let url = live_dto::build_url(endpoint, request.sample_rate_hz, "multi", &[]);
        assert!(
            url.query_pairs()
                .any(|pair| pair == ("sample_rate".into(), "16000".into()))
        );
        request.sample_rate_hz = 44_100;
        assert!(!profile_matches(&request));
        request.sample_rate_hz = 48_000;
        request.profile.language = "ru&model=attacker".into();
        assert!(!profile_matches(&request));
    }
    async fn spawn_success() -> (Url, tokio::task::JoinHandle<CapturedFlow>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (handshake_sender, handshake_receiver) = oneshot::channel();
            #[allow(clippy::result_large_err)]
            let callback = move |request: &Request, mut response: Response| {
                let captured = (
                    request.uri().to_string(),
                    request.headers()[AUTHORIZATION]
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
                handshake_sender.send(captured).unwrap();
                response
                    .headers_mut()
                    .insert("x-dg-request-id", HeaderValue::from_static("handshake-1"));
                Ok(response)
            };
            let mut socket = accept_hdr_async(stream, callback).await.unwrap();
            let (target, authorization) = handshake_receiver.await.unwrap();
            let audio = socket.next().await.unwrap().unwrap().into_data().to_vec();
            let finalize = socket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap()
                .to_string();
            for payload in [
                r#"{"type":"FutureEvent"}"#,
                r#"{"type":"Metadata","request_id":"metadata-2"}"#,
                r#"{"type":"Results","start":0.25,"duration":0.5,"channel":{"alternatives":[{"transcript":"hel","confidence":0.5}]}}"#,
                r#"{"type":"Results","start":0.25,"duration":0.75,"is_final":true,"speech_final":true,"from_finalize":true,"channel":{"alternatives":[{"transcript":"hello","confidence":0.9}]}}"#,
                r#"{"type":"UtteranceEnd","last_word_end":1.0}"#,
                r#"{"type":"Error","err_code":"BAD_REQUEST"}"#,
            ] {
                socket.send(Message::text(payload)).await.unwrap();
            }
            let close_stream = socket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap()
                .to_string();
            let saw_close = matches!(socket.next().await, Some(Ok(Message::Close(_))));
            CapturedFlow {
                target,
                authorization,
                audio,
                finalize,
                close_stream,
                saw_close,
            }
        });
        (
            Url::parse(&format!("ws://{address}/v1/listen?discarded=true")).unwrap(),
            handle,
        )
    }
    #[tokio::test]
    async fn sends_exact_handshake_audio_finalize_close_and_normalizes_events() {
        let (endpoint, server) = spawn_success().await;
        let factory = DeepgramLiveRecognizer::new("test-secret", endpoint).unwrap();
        let session = factory.open(recognition_request()).await.unwrap();
        session.write_audio(audio_frame()).await.unwrap();
        session.finalize().await.unwrap();
        let LiveRecognitionEvent::Transcript(partial) =
            session.next_event().await.unwrap().unwrap()
        else {
            panic!("expected partial transcript");
        };
        assert_eq!(partial.text, "hel");
        assert_eq!(partial.stability, LiveTranscriptStability::Partial);
        let LiveRecognitionEvent::Transcript(finalized) =
            session.next_event().await.unwrap().unwrap()
        else {
            panic!("expected final transcript");
        };
        assert_eq!(finalized.stability, LiveTranscriptStability::UtteranceFinal);
        assert_eq!(
            session.next_event().await.unwrap(),
            Some(LiveRecognitionEvent::FinalizeResultObserved)
        );
        assert_eq!(
            session.next_event().await.unwrap(),
            Some(LiveRecognitionEvent::UtteranceEnd {
                last_word_end_millis: 1_000,
            })
        );
        let error = session.next_event().await.unwrap_err();
        assert_eq!(
            error.class(),
            RecognitionFailureClass::KnownAcceptedTerminal
        );
        assert_eq!(error.provider_reference().unwrap().as_str(), "metadata-2");
        session.close().await.unwrap();
        let captured = server.await.unwrap();
        assert_eq!(captured.authorization, "Token test-secret");
        assert_eq!(captured.audio, vec![1, 2, 3, 4]);
        assert_eq!(captured.finalize, r#"{"type":"Finalize"}"#);
        assert_eq!(captured.close_stream, r#"{"type":"CloseStream"}"#);
        assert!(captured.saw_close);
        assert_eq!(
            captured.target,
            concat!(
                "/v1/listen?encoding=linear16&sample_rate=48000&channels=1&language=multi&",
                "model=nova-3&punctuate=true&interim_results=true&utterance_end_ms=1400&",
                "vad_events=true&endpointing=300&smart_format=true&no_delay=true&",
                "keyterm=Craig&keyterm=release+train"
            )
        );
        assert!(!format!("{factory:?}").contains("test-secret"));
    }
    async fn rejected_handshake(status: StatusCode) -> RecognitionFailure {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            #[allow(clippy::result_large_err)]
            let callback = move |_request: &Request, _response: Response| {
                let mut response = ErrorResponse::new(Some("rejected".into()));
                *response.status_mut() = status;
                let headers = response.headers_mut();
                headers.insert(RETRY_AFTER, HeaderValue::from_static("90"));
                headers.insert("x-dg-request-id", HeaderValue::from_static("reject-1"));
                Err(response)
            };
            let _expected_rejection = accept_hdr_async(stream, callback).await;
        });
        let endpoint = Url::parse(&format!("ws://{address}/v1/listen")).unwrap();
        let error = DeepgramLiveRecognizer::new("key", endpoint)
            .unwrap()
            .open(recognition_request())
            .await
            .err()
            .unwrap();
        server.await.unwrap();
        error
    }
    #[tokio::test]
    async fn classifies_handshake_status_and_rejects_profile_before_connect() {
        let limited = rejected_handshake(StatusCode::TOO_MANY_REQUESTS).await;
        assert_eq!(
            limited.class(),
            RecognitionFailureClass::KnownNotAccepted { retryable: true }
        );
        assert_eq!(limited.provider_reference().unwrap().as_str(), "reject-1");
        let RecognitionFailure::KnownNotAccepted {
            retry_after_millis, ..
        } = limited
        else {
            panic!("expected rejection");
        };
        assert_eq!(retry_after_millis, Some(MAX_RETRY_AFTER_MILLIS));
        let unavailable = rejected_handshake(StatusCode::SERVICE_UNAVAILABLE).await;
        assert_eq!(
            unavailable.class(),
            RecognitionFailureClass::KnownNotAccepted { retryable: true }
        );
        let auth = rejected_handshake(StatusCode::UNAUTHORIZED).await;
        assert_eq!(
            auth.class(),
            RecognitionFailureClass::KnownNotAccepted { retryable: false }
        );
        let endpoint = Url::parse("ws://127.0.0.1:9/v1/listen").unwrap();
        let factory = DeepgramLiveRecognizer::new("key", endpoint).unwrap();
        let mut request = recognition_request();
        request.sample_rate_hz = 44_100;
        assert_eq!(
            factory.open(request).await.err().unwrap().code(),
            "DEEPGRAM_LIVE_PROFILE_MISMATCH"
        );
    }
}
