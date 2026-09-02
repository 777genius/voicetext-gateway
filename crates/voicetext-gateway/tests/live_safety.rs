#[allow(dead_code, unused_imports)]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::SinkExt;
use serde_json::json;
use tokio::sync::Notify;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use voicetext_speech::application::ports::{
    BoxFuture, LiveAudioFrame, LiveRecognitionEvent, LiveRecognitionRequest, LiveRecognizerFactory,
    LiveRecognizerSession, RecognitionFailure,
};

use support::{TOKEN, TestGateway};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stall {
    None,
    Open,
    Write,
    Finalize,
    Close,
}

#[derive(Debug, Default)]
struct Calls {
    opened: AtomicUsize,
    writes: AtomicUsize,
    finalizes: AtomicUsize,
    closes: AtomicUsize,
}

#[derive(Debug)]
struct StallSignals {
    started: Notify,
    cancelled: Notify,
}

impl StallSignals {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Notify::new(),
            cancelled: Notify::new(),
        })
    }
}

#[derive(Debug)]
struct PendingGuard(Arc<StallSignals>);

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.cancelled.notify_one();
    }
}

async fn stall(signals: Arc<StallSignals>) {
    let _guard = PendingGuard(Arc::clone(&signals));
    signals.started.notify_one();
    std::future::pending::<()>().await;
}

#[derive(Debug)]
struct StallingFactory {
    stall: Stall,
    signals: Arc<StallSignals>,
    calls: Arc<Calls>,
}

impl LiveRecognizerFactory for StallingFactory {
    fn capabilities(
        &self,
    ) -> &'static voicetext_speech::application::live_capabilities::LiveCapabilityDescriptor {
        support::live_capabilities(voicetext_gateway::contracts::live::LiveIdentity::DeepgramNova3)
    }

    fn open(
        &self,
        _request: LiveRecognitionRequest,
    ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>> {
        Box::pin(async move {
            self.calls.opened.fetch_add(1, Ordering::SeqCst);
            if self.stall == Stall::Open {
                stall(Arc::clone(&self.signals)).await;
            }
            Ok(Box::new(StallingSession {
                stall: self.stall,
                signals: Arc::clone(&self.signals),
                calls: Arc::clone(&self.calls),
            }) as Box<dyn LiveRecognizerSession>)
        })
    }
}

#[derive(Debug)]
struct StallingSession {
    stall: Stall,
    signals: Arc<StallSignals>,
    calls: Arc<Calls>,
}

#[derive(Debug)]
struct ProviderErrorFactory;

impl LiveRecognizerFactory for ProviderErrorFactory {
    fn capabilities(
        &self,
    ) -> &'static voicetext_speech::application::live_capabilities::LiveCapabilityDescriptor {
        support::live_capabilities(voicetext_gateway::contracts::live::LiveIdentity::DeepgramNova3)
    }

    fn open(
        &self,
        _request: LiveRecognitionRequest,
    ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>> {
        Box::pin(async { Ok(Box::new(ProviderErrorSession) as Box<dyn LiveRecognizerSession>) })
    }
}

#[derive(Debug)]
struct ProviderErrorSession;

impl LiveRecognizerSession for ProviderErrorSession {
    fn write_audio(&self, _frame: LiveAudioFrame) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async { Ok(()) })
    }

    fn next_event(
        &self,
    ) -> BoxFuture<'_, Result<Option<LiveRecognitionEvent>, RecognitionFailure>> {
        Box::pin(async {
            Err(RecognitionFailure::KnownAcceptedTerminal {
                code: "SYNTHETIC_PROVIDER_TERMINAL".into(),
                provider_reference: None,
            })
        })
    }

    fn finalize(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async { Ok(()) })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async { Ok(()) })
    }
}

impl LiveRecognizerSession for StallingSession {
    fn write_audio(&self, _frame: LiveAudioFrame) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            self.calls.writes.fetch_add(1, Ordering::SeqCst);
            if self.stall == Stall::Write {
                stall(Arc::clone(&self.signals)).await;
            }
            Ok(())
        })
    }

    fn next_event(
        &self,
    ) -> BoxFuture<'_, Result<Option<LiveRecognitionEvent>, RecognitionFailure>> {
        Box::pin(std::future::pending())
    }

    fn finalize(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            self.calls.finalizes.fetch_add(1, Ordering::SeqCst);
            if self.stall == Stall::Finalize {
                stall(Arc::clone(&self.signals)).await;
            }
            Ok(())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
        Box::pin(async move {
            self.calls.closes.fetch_add(1, Ordering::SeqCst);
            if self.stall == Stall::Close {
                stall(Arc::clone(&self.signals)).await;
            }
            Ok(())
        })
    }
}

async fn connect(stall_kind: Stall) -> (TestGateway, Socket, Arc<StallSignals>, Arc<Calls>) {
    let signals = StallSignals::new();
    let calls = Arc::new(Calls::default());
    let factory = Arc::new(StallingFactory {
        stall: stall_kind,
        signals: Arc::clone(&signals),
        calls: Arc::clone(&calls),
    });
    let gateway = TestGateway::start_with_live(factory).await;
    let socket = connect_to(&gateway).await;
    (gateway, socket, signals, calls)
}

async fn configure(socket: &mut Socket) {
    socket
        .send(Message::Text(
            json!({
                "type": "config",
                "provider": "deepgram",
                "model": "nova-3",
                "language": "multi",
                "capabilities": ["finalize_ack"],
                "channels": 1,
                "protocol_v": 2,
                "client_session_id": "123e4567-e89b-42d3-a456-426614174000",
                "encoding": "pcm_s16le",
                "sample_rate": 16000,
                "keyterms": []
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn connect_to(gateway: &TestGateway) -> Socket {
    let mut request = gateway.websocket_url().into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap(),
    );
    let (mut socket, _) = connect_async(request).await.unwrap();
    configure(&mut socket).await;
    socket
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[tokio::test]
async fn provider_open_stall_is_cancelled_at_the_gateway_bound() {
    let (gateway, socket, signals, calls) = connect(Stall::Open).await;
    signals.started.notified().await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    signals.cancelled.notified().await;
    assert_eq!(calls.opened.load(Ordering::SeqCst), 1);
    drop(socket);
    gateway.stop().await;
}

#[tokio::test]
async fn idle_and_finalize_drain_deadlines_terminate_without_sleeping() {
    let (gateway, mut socket, _signals, _calls) = connect(Stall::None).await;
    let _ready = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    let timeout_frame = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    let timeout_json: serde_json::Value =
        serde_json::from_str(timeout_frame.into_text().unwrap().as_str()).unwrap();
    assert_eq!(timeout_json["type"], "error");
    let closed = futures_util::StreamExt::next(&mut socket).await;
    assert!(matches!(
        closed,
        None | Some(Err(_) | Ok(Message::Close(_)))
    ));
    drop(socket);
    gateway.stop().await;
    tokio::time::resume();

    let (gateway, mut socket, _signals, calls) = connect(Stall::None).await;
    let _ready = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    socket
        .send(Message::Binary(vec![0, 0].into()))
        .await
        .unwrap();
    tokio::time::pause();
    let _ack = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    socket
        .send(Message::Text(r#"{"type":"finalize"}"#.into()))
        .await
        .unwrap();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let finalized = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    assert!(finalized.is_text());
    assert_eq!(calls.finalizes.load(Ordering::SeqCst), 1);
    drop(socket);
    gateway.stop().await;
}

#[tokio::test]
async fn provider_error_and_queued_client_frames_emit_one_protocol_terminal() {
    let gateway = TestGateway::start_with_live(Arc::new(ProviderErrorFactory)).await;
    let mut socket = connect_to(&gateway).await;
    let ready = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    assert!(ready.is_text());

    let _ = socket.send(Message::Binary(vec![0, 0].into())).await;
    let _ = socket
        .send(Message::Text(r#"{"type":"finalize"}"#.into()))
        .await;
    let _ = socket
        .send(Message::Text(r#"{"type":"close"}"#.into()))
        .await;

    let mut terminals = 0;
    loop {
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut socket),
        )
        .await
        .expect("gateway did not terminate provider-error race");
        match next {
            Some(Ok(Message::Text(text))) => {
                let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                terminals += usize::from(matches!(
                    value["type"].as_str(),
                    Some("error" | "finalize_complete")
                ));
            }
            None | Some(Err(_) | Ok(Message::Close(_))) => break,
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_))) => {}
            Some(Ok(Message::Frame(_))) => unreachable!("raw frame is not exposed by tungstenite"),
        }
    }
    assert_eq!(terminals, 1);
    gateway.stop().await;
}

#[tokio::test]
async fn client_disconnect_cancels_a_stalled_write_and_closes_once() {
    let (gateway, mut socket, signals, calls) = connect(Stall::Write).await;
    let _ready = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    socket
        .send(Message::Binary(vec![0, 0].into()))
        .await
        .unwrap();
    signals.started.notified().await;
    socket.send(Message::Close(None)).await.unwrap();
    signals.cancelled.notified().await;
    while calls.closes.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.writes.load(Ordering::SeqCst), 1);
    assert_eq!(calls.closes.load(Ordering::SeqCst), 1);
    drop(socket);
    gateway.stop().await;
}

#[tokio::test]
async fn client_disconnect_cancels_one_finalize_before_one_close() {
    let (gateway, mut socket, signals, calls) = connect(Stall::Finalize).await;
    let _ready = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    socket
        .send(Message::Binary(vec![0, 0].into()))
        .await
        .unwrap();
    let _ack = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    socket
        .send(Message::Text(r#"{"type":"finalize"}"#.into()))
        .await
        .unwrap();
    signals.started.notified().await;
    socket.send(Message::Close(None)).await.unwrap();
    signals.cancelled.notified().await;
    while calls.closes.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.finalizes.load(Ordering::SeqCst), 1);
    assert_eq!(calls.closes.load(Ordering::SeqCst), 1);
    drop(socket);
    gateway.stop().await;
}

#[tokio::test]
async fn close_stall_is_cancelled_at_the_gateway_bound() {
    let (gateway, mut socket, signals, calls) = connect(Stall::Close).await;
    let _ready = futures_util::StreamExt::next(&mut socket)
        .await
        .unwrap()
        .unwrap();
    socket
        .send(Message::Text(r#"{"type":"close"}"#.into()))
        .await
        .unwrap();
    signals.started.notified().await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    signals.cancelled.notified().await;
    assert_eq!(calls.closes.load(Ordering::SeqCst), 1);
    drop(socket);
    gateway.stop().await;
}
