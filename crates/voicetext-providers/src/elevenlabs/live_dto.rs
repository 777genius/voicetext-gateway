use base64::Engine;
use serde_json::Value;
use url::Url;

pub(super) const MAX_PROVIDER_MESSAGE_BYTES: usize = 1_024 * 1_024;
const MAX_TRANSCRIPT_BYTES: usize = 64 * 1_024;
const MALFORMED_RESPONSE: &str = "ELEVENLABS_LIVE_MALFORMED_RESPONSE";
const REALTIME_MODEL: &str = "scribe_v2_realtime";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TranscriptKind {
    Partial,
    SegmentFinal,
    UtteranceFinal,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ParsedLiveMessage {
    SessionStarted(Option<String>),
    Transcript {
        kind: TranscriptKind,
        text: String,
        confidence: Option<f32>,
    },
    TerminalError {
        code: &'static str,
    },
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ParseFailure {
    pub code: &'static str,
}

pub(super) fn encode_audio(audio: &[u8], sample_rate_hz: u32, commit: bool) -> String {
    serde_json::json!({
        "message_type": "input_audio_chunk",
        "audio_base_64": base64::engine::general_purpose::STANDARD.encode(audio),
        "sample_rate": sample_rate_hz,
        "commit": commit,
    })
    .to_string()
}

pub(super) fn valid_language_code(language: &str) -> bool {
    (1..=10).contains(&language.len())
        && language
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(super) fn build_url(
    mut endpoint: Url,
    sample_rate_hz: u32,
    language: &str,
    keyterms: &[String],
) -> Url {
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    let mut query = endpoint.query_pairs_mut();
    for (name, value) in [
        ("model_id", REALTIME_MODEL),
        ("audio_format", &format!("pcm_{sample_rate_hz}")),
        // Scribe realtime does not correlate committed transcripts with the request that
        // committed them. Manual commit is therefore required: with VAD enabled a delayed
        // automatic result could be mistaken for the explicit-finalize result.
        ("commit_strategy", "manual"),
        ("include_timestamps", "false"),
    ] {
        query.append_pair(name, value);
    }
    if language == "multi" {
        query.append_pair("include_language_detection", "true");
    } else {
        query.append_pair("language_code", language);
    }
    for keyterm in keyterms {
        query.append_pair("keyterms", keyterm);
    }
    drop(query);
    endpoint
}

pub(super) fn parse_text(payload: &str) -> Result<ParsedLiveMessage, ParseFailure> {
    if payload.len() > MAX_PROVIDER_MESSAGE_BYTES {
        return Err(malformed());
    }
    let value: Value = serde_json::from_str(payload).map_err(|_| malformed())?;
    let object = value.as_object().ok_or_else(malformed)?;
    let message_type = object
        .get("message_type")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    match message_type {
        "session_started" => Ok(ParsedLiveMessage::SessionStarted(
            object
                .get("session_id")
                .and_then(Value::as_str)
                .and_then(safe_reference),
        )),
        "partial_transcript" => parse_transcript(object, TranscriptKind::Partial),
        "final_transcript" | "final_transcript_with_timestamps" => {
            parse_transcript(object, TranscriptKind::SegmentFinal)
        }
        "committed_transcript" | "committed_transcript_with_timestamps" => {
            parse_transcript(object, TranscriptKind::UtteranceFinal)
        }
        kind => Ok(match terminal_error_code(kind) {
            Some(code) => ParsedLiveMessage::TerminalError { code },
            None => ParsedLiveMessage::Ignore,
        }),
    }
}

fn parse_transcript(
    object: &serde_json::Map<String, Value>,
    kind: TranscriptKind,
) -> Result<ParsedLiveMessage, ParseFailure> {
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| text.len() <= MAX_TRANSCRIPT_BYTES)
        .ok_or_else(malformed)?
        .to_owned();
    let confidence = parse_confidence(object.get("confidence"))?;
    if text.trim().is_empty() && kind != TranscriptKind::UtteranceFinal {
        return Ok(ParsedLiveMessage::Ignore);
    }
    Ok(ParsedLiveMessage::Transcript {
        kind,
        text,
        confidence,
    })
}

fn parse_confidence(value: Option<&Value>) -> Result<Option<f32>, ParseFailure> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value::<f32>(value.clone())
            .ok()
            .filter(|confidence| confidence.is_finite() && (0.0..=1.0).contains(confidence))
            .map(Some)
            .ok_or_else(malformed),
    }
}

fn terminal_error_code(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "auth_error" => "ELEVENLABS_LIVE_AUTH_ERROR",
        "quota_exceeded" => "ELEVENLABS_LIVE_QUOTA_EXCEEDED",
        "transcriber_error" => "ELEVENLABS_LIVE_TRANSCRIBER_ERROR",
        "input_error" => "ELEVENLABS_LIVE_INPUT_ERROR",
        "invalid_request" => "ELEVENLABS_LIVE_INVALID_REQUEST",
        "error" => "ELEVENLABS_LIVE_PROVIDER_ERROR",
        "commit_throttled" | "scribe_throttled_error" => "ELEVENLABS_LIVE_COMMIT_THROTTLED",
        "unaccepted_terms" => "ELEVENLABS_LIVE_UNACCEPTED_TERMS",
        "rate_limited" | "scribe_rate_limited_error" => "ELEVENLABS_LIVE_RATE_LIMITED",
        "queue_overflow" | "scribe_queue_overflow_error" => "ELEVENLABS_LIVE_QUEUE_OVERFLOW",
        "resource_exhausted" => "ELEVENLABS_LIVE_RESOURCE_EXHAUSTED",
        "session_time_limit_exceeded" => "ELEVENLABS_LIVE_SESSION_TIME_LIMIT_EXCEEDED",
        "chunk_size_exceeded" => "ELEVENLABS_LIVE_CHUNK_SIZE_EXCEEDED",
        "insufficient_audio_activity" => "ELEVENLABS_LIVE_INSUFFICIENT_AUDIO_ACTIVITY",
        other if other.contains("error") => "ELEVENLABS_LIVE_PROVIDER_ERROR",
        _ => return None,
    })
}

pub(super) fn safe_reference(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.trim() == value
        && value.len() <= 128
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)))
    .then(|| value.to_owned())
}

const fn malformed() -> ParseFailure {
    ParseFailure {
        code: MALFORMED_RESPONSE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transcripts_without_treating_provider_timing_as_authoritative() {
        let ParsedLiveMessage::Transcript {
            kind,
            text,
            confidence,
        } = parse_text(
            r#"{"message_type":"final_transcript_with_timestamps","text":"hello","confidence":0.8,"words":[{"start":0.0,"end":1.25}]}"#,
        )
        .unwrap()
        else {
            panic!("expected transcript");
        };
        assert_eq!(kind, TranscriptKind::SegmentFinal);
        assert_eq!(text, "hello");
        assert_eq!(confidence, Some(0.8));
    }

    #[test]
    fn accepts_only_exact_bounded_printable_ascii_references() {
        assert_eq!(safe_reference("session-1").as_deref(), Some("session-1"));
        for rejected in [
            "",
            " session-1",
            "session-1 ",
            "bad\nid",
            "bad\tid",
            "не-ascii",
        ] {
            assert!(safe_reference(rejected).is_none(), "accepted {rejected:?}");
        }
        assert!(safe_reference(&"x".repeat(129)).is_none());
    }

    #[test]
    fn ignores_missing_nullable_leading_and_decreasing_provider_timing() {
        for payload in [
            r#"{"message_type":"committed_transcript","text":"hello"}"#,
            r#"{"message_type":"committed_transcript_with_timestamps","text":"hello","words":null}"#,
            r#"{"message_type":"committed_transcript_with_timestamps","text":"hello","words":[{"start":null,"end":null}]}"#,
            r#"{"message_type":"committed_transcript_with_timestamps","text":"hello","words":[{"start":8.0,"end":9.0}]}"#,
            r#"{"message_type":"committed_transcript_with_timestamps","text":"hello","words":[{"end":2.0},{"end":1.0}]}"#,
        ] {
            assert!(matches!(
                parse_text(payload).unwrap(),
                ParsedLiveMessage::Transcript {
                    kind: TranscriptKind::UtteranceFinal,
                    ..
                }
            ));
        }
    }

    #[test]
    fn recognizes_errors_and_rejects_unbounded_or_invalid_data() {
        for (kind, code) in [
            ("auth_error", "ELEVENLABS_LIVE_AUTH_ERROR"),
            ("quota_exceeded", "ELEVENLABS_LIVE_QUOTA_EXCEEDED"),
            ("input_error", "ELEVENLABS_LIVE_INPUT_ERROR"),
            ("invalid_request", "ELEVENLABS_LIVE_INVALID_REQUEST"),
            ("unaccepted_terms", "ELEVENLABS_LIVE_UNACCEPTED_TERMS"),
            ("rate_limited", "ELEVENLABS_LIVE_RATE_LIMITED"),
            ("scribe_throttled_error", "ELEVENLABS_LIVE_COMMIT_THROTTLED"),
            (
                "insufficient_audio_activity",
                "ELEVENLABS_LIVE_INSUFFICIENT_AUDIO_ACTIVITY",
            ),
        ] {
            assert_eq!(
                parse_text(&format!(
                    r#"{{"message_type":"{kind}","error":"redacted"}}"#
                ))
                .unwrap(),
                ParsedLiveMessage::TerminalError { code }
            );
        }
        for payload in [
            r#"{"message_type":"partial_transcript","text":"x","confidence":2}"#.to_owned(),
            format!(
                r#"{{"message_type":"future","padding":"{}"}}"#,
                "x".repeat(MAX_PROVIDER_MESSAGE_BYTES)
            ),
        ] {
            assert_eq!(parse_text(&payload), Err(malformed()));
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
    use url::Url;
    use voicetext_speech::application::ports::{
        LiveProfile, LiveRecognitionRequest, LiveRecognizerFactory, LiveRecognizerSession,
        RecognitionFailure, RecognitionFailureClass,
    };

    use super::super::live::ElevenLabsLiveRecognizer;
    use super::{build_url, valid_language_code};

    fn request() -> LiveRecognitionRequest {
        LiveRecognitionRequest {
            profile: LiveProfile {
                protocol_version: 2,
                provider: "elevenlabs".into(),
                model: "scribe_v2_realtime".into(),
                language: "multi".into(),
            },
            sample_rate_hz: 48_000,
            channels: 1,
            keyterms: Vec::new(),
        }
    }

    #[tokio::test]
    async fn validates_language_codes_profiles_and_explicit_or_multi_url() {
        for valid in ["ru", "en-US", "multi", "pt-BR"] {
            assert!(valid_language_code(valid));
        }
        for invalid in ["", "ru&key=x", "with space", "abcdefghijk", "ки"] {
            assert!(!valid_language_code(invalid));
        }
        let endpoint = Url::parse("wss://example.test/realtime?old=true").unwrap();
        let explicit = build_url(endpoint.clone(), 48_000, "pt-BR", &[]);
        assert!(
            explicit
                .query_pairs()
                .any(|pair| pair == ("language_code".into(), "pt-BR".into()))
        );
        assert!(
            !explicit
                .query_pairs()
                .any(|(key, _)| key == "include_language_detection")
        );
        let multi = build_url(endpoint, 48_000, "multi", &[]);
        assert!(
            multi
                .query_pairs()
                .any(|pair| { pair == ("include_language_detection".into(), "true".into()) })
        );
        assert!(!multi.query_pairs().any(|(key, _)| key == "language_code"));
        let factory =
            ElevenLabsLiveRecognizer::new("key", Url::parse("ws://127.0.0.1:9/realtime").unwrap())
                .unwrap();
        let mut invalid = request();
        invalid.profile.language = "ru&key=x".into();
        let Err(failure) = factory.open(invalid).await else {
            panic!("expected profile rejection");
        };
        assert_eq!(failure.code(), "ELEVENLABS_LIVE_PROFILE_MISMATCH");
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // Imposed by tungstenite's test handshake callback.
    async fn classifies_handshake_status_and_post_open_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let rejected = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let callback = |_request: &Request, _response: Response| {
                let mut response = ErrorResponse::new(Some("limited".into()));
                *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
                response
                    .headers_mut()
                    .insert("request-id", HeaderValue::from_static("reject-1"));
                Err(response)
            };
            let _ = accept_hdr_async(stream, callback).await;
        });
        let endpoint = Url::parse(&format!("ws://{address}/realtime")).unwrap();
        let result = ElevenLabsLiveRecognizer::new("key", endpoint)
            .unwrap()
            .open(request())
            .await;
        let Err(failure) = result else {
            panic!("expected rejected handshake");
        };
        rejected.await.unwrap();
        assert_eq!(
            failure.class(),
            RecognitionFailureClass::KnownNotAccepted { retryable: true }
        );
        assert_eq!(failure.provider_reference().unwrap().as_str(), "reject-1");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let closed = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket =
                accept_hdr_async(stream, |_: &Request, response: Response| Ok(response))
                    .await
                    .unwrap();
            socket.close(None).await.unwrap();
        });
        let endpoint = Url::parse(&format!("ws://{address}/realtime")).unwrap();
        let session: Box<dyn LiveRecognizerSession> =
            ElevenLabsLiveRecognizer::new("key", endpoint)
                .unwrap()
                .open(request())
                .await
                .unwrap();
        assert!(matches!(
            session.next_event().await,
            Err(RecognitionFailure::UnknownAfterSend { .. })
        ));
        closed.await.unwrap();
    }
}
