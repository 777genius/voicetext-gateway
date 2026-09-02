use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub const DEEPGRAM_KEY: &str = "deepgram-e2e-secret";
pub const ELEVENLABS_KEY: &str = "elevenlabs-e2e-secret";

#[derive(Clone, Debug, Default)]
pub struct ProviderWireCounters {
    deepgram_batch: Arc<AtomicUsize>,
    deepgram_live: Arc<AtomicUsize>,
    elevenlabs_batch: Arc<AtomicUsize>,
    elevenlabs_live: Arc<AtomicUsize>,
}

impl ProviderWireCounters {
    pub fn snapshot(&self) -> [usize; 4] {
        [
            self.deepgram_batch.load(Ordering::SeqCst),
            self.deepgram_live.load(Ordering::SeqCst),
            self.elevenlabs_batch.load(Ordering::SeqCst),
            self.elevenlabs_live.load(Ordering::SeqCst),
        ]
    }
}

pub struct RunningProviderWire {
    address: SocketAddr,
    counters: ProviderWireCounters,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningProviderWire {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let counters = ProviderWireCounters::default();
        let router = Router::new()
            .route("/deepgram/batch", post(deepgram_batch))
            .route("/deepgram/live", any(deepgram_live))
            .route("/elevenlabs/batch", post(elevenlabs_batch))
            .route("/elevenlabs/live", any(elevenlabs_live))
            .with_state(counters.clone());
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _received = receiver.await;
                })
                .await
                .unwrap();
        });
        Self {
            address,
            counters,
            shutdown: Some(shutdown),
            task,
        }
    }

    pub fn deepgram_batch_endpoint(&self) -> String {
        format!("http://{}/deepgram/batch", self.address)
    }

    pub fn deepgram_live_endpoint(&self) -> String {
        format!("ws://{}/deepgram/live", self.address)
    }

    pub fn elevenlabs_batch_endpoint(&self) -> String {
        format!("http://{}/elevenlabs/batch", self.address)
    }

    pub fn elevenlabs_live_endpoint(&self) -> String {
        format!("ws://{}/elevenlabs/live", self.address)
    }

    pub fn counters(&self) -> ProviderWireCounters {
        self.counters.clone()
    }

    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _sent = shutdown.send(());
        }
        self.task.await.unwrap();
    }
}

async fn deepgram_batch(
    State(counters): State<ProviderWireCounters>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    assert_eq!(
        headers["authorization"].to_str().unwrap(),
        format!("Token {DEEPGRAM_KEY}")
    );
    assert!(!body.is_empty());
    counters.deepgram_batch.fetch_add(1, Ordering::SeqCst);
    (
        [("x-dg-request-id", "deepgram-batch-e2e")],
        axum::Json(json!({
            "metadata": {"duration": 0.02, "request_id": "deepgram-batch-e2e"},
            "results": {
                "channels": [{"alternatives": [{
                    "transcript": "synthetic speech",
                    "paragraphs": {"paragraphs": [{"sentences": [{
                        "text": "synthetic speech", "start": 0.0, "end": 0.02
                    }]}]}
                }]}],
                "utterances": [{
                    "start": 0.0, "end": 0.02, "transcript": "synthetic speech",
                    "confidence": 0.9, "speaker": 0
                }]
            }
        })),
    )
}

async fn elevenlabs_batch(
    State(counters): State<ProviderWireCounters>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    assert_eq!(headers["xi-api-key"].to_str().unwrap(), ELEVENLABS_KEY);
    assert!(!body.is_empty());
    counters.elevenlabs_batch.fetch_add(1, Ordering::SeqCst);
    axum::Json(json!({
        "language_code": "en",
        "language_probability": 0.99,
        "audio_duration_secs": 0.02,
        "transcription_id": "elevenlabs-batch-e2e",
        "text": "synthetic speech",
        "words": [
            {"type": "word", "text": "synthetic", "start": 0.0, "end": 0.01, "logprob": -0.1},
            {"type": "spacing", "text": " "},
            {"type": "word", "text": "speech", "start": 0.01, "end": 0.02, "logprob": -0.1}
        ]
    }))
}

async fn deepgram_live(
    websocket: WebSocketUpgrade,
    State(counters): State<ProviderWireCounters>,
    headers: HeaderMap,
) -> Response {
    assert_eq!(
        headers["authorization"].to_str().unwrap(),
        format!("Token {DEEPGRAM_KEY}")
    );
    counters.deepgram_live.fetch_add(1, Ordering::SeqCst);
    websocket.on_upgrade(deepgram_live_session)
}

async fn deepgram_live_session(mut socket: WebSocket) {
    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Binary(audio) => assert!(!audio.is_empty()),
            Message::Text(command) if command.contains("Finalize") => {
                for event in [
                    json!({
                        "type": "Results", "start": 0.02, "duration": 0.04,
                        "is_final": false,
                        "channel": {"alternatives": [{
                            "transcript": "synthetic live speech", "confidence": 0.9
                        }]}
                    }),
                    json!({
                        "type": "Results", "start": 0.02, "duration": 0.04,
                        "is_final": true, "speech_final": true, "from_finalize": true,
                        "channel": {"alternatives": [{
                            "transcript": "synthetic live speech", "confidence": 0.9
                        }]}
                    }),
                ] {
                    socket
                        .send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
            }
            Message::Text(command) if command.contains("CloseStream") => break,
            Message::Close(_) => break,
            Message::Ping(payload) => {
                socket.send(Message::Pong(payload)).await.unwrap();
            }
            Message::Text(_) | Message::Pong(_) => {}
        }
    }
}

async fn elevenlabs_live(
    websocket: WebSocketUpgrade,
    State(counters): State<ProviderWireCounters>,
    headers: HeaderMap,
) -> Response {
    assert_eq!(headers["xi-api-key"].to_str().unwrap(), ELEVENLABS_KEY);
    counters.elevenlabs_live.fetch_add(1, Ordering::SeqCst);
    websocket.on_upgrade(elevenlabs_live_session)
}

async fn elevenlabs_live_session(mut socket: WebSocket) {
    socket
        .send(Message::Text(
            json!({"message_type": "session_started", "session_id": "elevenlabs-live-e2e"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let mut audio_messages = 0_usize;
    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(payload) if payload.contains("input_audio_chunk") => {
                audio_messages += 1;
                if audio_messages == 4 {
                    for kind in [
                        "partial_transcript",
                        "final_transcript",
                        "committed_transcript",
                    ] {
                        let event = json!({
                            "message_type": kind,
                            "text": "synthetic live speech",
                            "confidence": 0.9,
                            "words": [{"start": 0.0, "end": 0.06}]
                        });
                        socket
                            .send(Message::Text(event.to_string().into()))
                            .await
                            .unwrap();
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(payload) => {
                socket.send(Message::Pong(payload)).await.unwrap();
            }
            Message::Binary(_) | Message::Text(_) | Message::Pong(_) => {}
        }
    }
}
