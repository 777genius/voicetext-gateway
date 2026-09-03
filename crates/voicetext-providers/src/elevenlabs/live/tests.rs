use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use voicetext_speech::application::ports::{
    LiveProfile, LiveTranscriptStability, RecognitionFailureClass,
};
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
        keyterms: vec!["Craig".into(), "Craig".into(), "release train".into()],
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
        // Model the adversarial ordering from the rejected VAD strategy: an automatic commit can
        // be delayed in the wire queue until immediately before the explicit commit result.
        if target.contains("commit_strategy=vad") {
            socket
                .send(Message::text(
                    r#"{"message_type":"committed_transcript","text":"delayed automatic"}"#,
                ))
                .await
                .unwrap();
        }
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
fn canonicalizes_keyterms_after_checked_validation() {
    let input = vec!["Craig".into(), "Craig".into(), "release   train".into()];
    let keyterms = canonicalize_keyterms(&input);
    assert_eq!(keyterms.len(), 2);
    assert_eq!(keyterms[0], "Craig");
    assert_eq!(keyterms[1], "release train");
}

#[tokio::test]
async fn manual_commit_prevents_delayed_vad_from_preceding_the_explicit_result() {
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
            .any(|pair| pair == ("commit_strategy".into(), "manual".into()))
    );
    assert!(
        !url.query_pairs()
            .any(|(key, _)| key == "vad_silence_threshold_secs")
    );
    assert_eq!(
        url.query_pairs()
            .filter(|(key, _)| key == "keyterms")
            .count(),
        2
    );
    assert!(!format!("{factory:?}").contains("test-secret"));
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
    let factory = ElevenLabsLiveRecognizer::new("key", endpoint).unwrap();
    let opening =
        tokio::spawn(async move { factory.open(recognition_request(16_000, "multi")).await });
    // Poll the spawned task once so the TCP handshake and timeout registration have started.
    tokio::task::yield_now().await;
    accepted_rx.await.unwrap();
    // Let the spawned client poll through the handshake and arm its timeout before advancing
    // the paused clock. Without these scheduling points the exact-boundary assertion races with
    // task startup and can leave the test waiting forever for a timer that was never registered.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(CONNECT_TIMEOUT).await;
    let error = opening.await.unwrap().err().unwrap();
    assert_eq!(error.code(), "ELEVENLABS_LIVE_CONNECT_TIMEOUT");
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
        (WRITE_TIMEOUT, "ELEVENLABS_LIVE_AUDIO_WRITE_TIMEOUT"),
        (WRITE_TIMEOUT, "ELEVENLABS_LIVE_FINALIZE_WRITE_TIMEOUT"),
        (CLOSE_TIMEOUT, "ELEVENLABS_LIVE_CLOSE_TIMEOUT"),
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
