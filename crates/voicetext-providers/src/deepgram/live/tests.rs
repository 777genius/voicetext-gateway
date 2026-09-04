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
        keyterms: vec!["Craig".into(), "release train".into()],
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
    let LiveRecognitionEvent::Transcript(partial) = session.next_event().await.unwrap().unwrap()
    else {
        panic!("expected partial transcript");
    };
    assert_eq!(partial.text, "hel");
    assert_eq!(partial.stability, LiveTranscriptStability::Partial);
    let LiveRecognitionEvent::Transcript(finalized) = session.next_event().await.unwrap().unwrap()
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
    let operation = session.provider_operation().await.unwrap();
    assert_eq!(operation.kind(), ProviderOperationKind::RequestId);
    assert_eq!(operation.id(), "metadata-2");
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
        assert!(accept_hdr_async(stream, callback).await.is_err());
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
    assert!(capabilities_match(&recognition_request()));
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

#[tokio::test(start_paused = true)]
async fn connect_stall_is_cancelled_at_the_exact_bound() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        accepted_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let endpoint = Url::parse(&format!("ws://{address}/v1/listen")).unwrap();
    let factory = DeepgramLiveRecognizer::new("key", endpoint).unwrap();
    let opening = tokio::spawn(async move { factory.open(recognition_request()).await });
    accepted_rx.await.unwrap();
    tokio::time::advance(CONNECT_TIMEOUT).await;
    let error = opening.await.unwrap().err().unwrap();
    assert_eq!(error.code(), "DEEPGRAM_LIVE_CONNECT_TIMEOUT");
    assert_eq!(
        error.class(),
        RecognitionFailureClass::KnownNotAccepted { retryable: true }
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn write_finalize_and_close_stalls_are_cancelled_at_their_bounds() {
    for (maximum, code) in [
        (WRITE_TIMEOUT, "DEEPGRAM_LIVE_AUDIO_WRITE_TIMEOUT"),
        (WRITE_TIMEOUT, "DEEPGRAM_LIVE_FINALIZE_WRITE_TIMEOUT"),
        (CLOSE_TIMEOUT, "DEEPGRAM_LIVE_CLOSE_TIMEOUT"),
    ] {
        let (started_tx, started_rx) = oneshot::channel();
        let operation = tokio::spawn(async move {
            bounded_io(
                maximum,
                async move {
                    started_tx.send(()).unwrap();
                    std::future::pending::<Result<(), WebSocketError>>().await
                },
                code,
                "UNEXPECTED_FAILURE",
            )
            .await
        });
        started_rx.await.unwrap();
        tokio::time::advance(maximum).await;
        assert_eq!(operation.await.unwrap().unwrap_err().code(), code);
    }
}
