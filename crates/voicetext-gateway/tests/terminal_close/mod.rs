use super::*;
use futures_util::StreamExt;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use voicetext_providers::deepgram::DeepgramLiveRecognizer;

async fn next(socket: &mut Socket) -> Message {
    tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("gateway frame deadline")
        .expect("gateway disconnected before terminal close")
        .expect("gateway WebSocket error")
}

async fn text_frame(socket: &mut Socket) -> serde_json::Value {
    let Message::Text(text) = next(socket).await else {
        panic!("expected protocol message before close");
    };
    serde_json::from_str(&text).unwrap()
}

// Real adapter, synthetic PCM and loopback provider wire only. The wire asserts
// exactly one audio write, one Finalize, and one CloseStream, in that order.
async fn wire_session(flushed: bool, acknowledge_close: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/listen", listener.local_addr().unwrap());
    let provider =
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut wire = accept_async(stream).await.unwrap();
            assert_eq!(
                wire.next().await.unwrap().unwrap(),
                Message::Binary(vec![0; 640].into())
            );
            let finalize = wire.next().await.unwrap().unwrap().into_text().unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&finalize).unwrap(),
                json!({"type": "Finalize"})
            );
            if flushed {
                wire.send(Message::Text(json!({
                "type": "Results", "start": 0.0, "duration": 0.02,
                "is_final": true, "speech_final": true, "from_finalize": true,
                "channel": {"alternatives": [{"transcript": "synthetic", "confidence": 0.9}]}
            }).to_string().into())).await.unwrap();
            }
            let close = wire.next().await.unwrap().unwrap().into_text().unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&close).unwrap(),
                json!({"type": "CloseStream"})
            );
            assert!(matches!(wire.next().await, Some(Ok(Message::Close(_)))));
            wire.flush().await.unwrap();
        });
    let factory =
        DeepgramLiveRecognizer::new("synthetic-test-key", endpoint.parse().unwrap()).unwrap();
    let gateway = TestGateway::start_with_live(Arc::new(factory)).await;
    let mut socket = connect_to(&gateway).await;
    assert_eq!(text_frame(&mut socket).await["type"], "ready");
    socket
        .send(Message::Binary(vec![0; 640].into()))
        .await
        .unwrap();
    assert_eq!(
        text_frame(&mut socket).await,
        json!({"type": "ack", "seq": 1})
    );
    socket
        .send(Message::Text(r#"{"type":"finalize"}"#.into()))
        .await
        .unwrap();
    if flushed {
        let transcript = text_frame(&mut socket).await;
        assert_eq!(transcript["type"], "final");
    }
    let finalized = text_frame(&mut socket).await;
    assert_eq!(finalized["type"], "finalize_complete");
    assert_eq!(
        finalized["status"],
        if flushed { "flushed" } else { "timeout" }
    );
    assert_eq!(finalized["saw_result"], flushed);
    let Message::Close(frame) = next(&mut socket).await else {
        panic!("finalize_complete must be followed by a close frame");
    };
    if flushed {
        assert_eq!(
            frame.expect("explicit close code required").code,
            CloseCode::Normal
        );
    } else {
        assert!(frame.is_none_or(|frame| frame.code != CloseCode::Normal));
    }
    finish_handshake(&mut socket, acknowledge_close).await;
    tokio::time::timeout(Duration::from_secs(2), provider)
        .await
        .unwrap()
        .unwrap();
    drop(socket);
    gateway.stop().await;
}

async fn finish_handshake(socket: &mut Socket, acknowledge_close: bool) {
    if acknowledge_close {
        socket.flush().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .is_none(),
            "server must finish the handshake"
        );
    } else {
        // Do not flush tungstenite's queued close reply. The gateway must drop
        // its transport at the cleanup deadline even with an unresponsive peer.
        tokio::time::pause();
        let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = socket.get_mut() else {
            panic!("fixture must use plain loopback TCP");
        };
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_secs(1), tcp.read(&mut byte))
                .await
                .is_err(),
            "server must wait for the peer close reply"
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(tcp.read(&mut byte).await.unwrap(), 0);
        tokio::time::resume();
    }
}

#[tokio::test]
async fn provider_wire_finalize_complete_precedes_explicit_normal_close() {
    tokio::time::timeout(Duration::from_secs(10), wire_session(true, true))
        .await
        .expect("wire session deadline");
}

#[tokio::test]
async fn provider_wire_finalize_timeout_does_not_close_normally() {
    tokio::time::timeout(Duration::from_secs(10), wire_session(false, true))
        .await
        .expect("wire session deadline");
}

#[tokio::test]
async fn successful_close_cleanup_is_bounded_without_peer_reply() {
    tokio::time::timeout(Duration::from_secs(10), wire_session(true, false))
        .await
        .expect("wire session deadline");
}

#[tokio::test]
async fn no_audio_finalize_closes_normally_without_claiming_provider_result() {
    let (gateway, mut socket, _signals, calls) = connect(Stall::None).await;
    assert_eq!(text_frame(&mut socket).await["type"], "ready");
    socket
        .send(Message::Text(r#"{"type":"finalize"}"#.into()))
        .await
        .unwrap();
    calls.finalize_drain_started.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    let finalized = text_frame(&mut socket).await;
    assert_eq!(finalized["type"], "finalize_complete");
    assert_eq!(finalized["status"], "no_provider");
    assert_eq!(finalized["saw_result"], false);
    assert!(
        matches!(next(&mut socket).await, Message::Close(Some(frame)) if frame.code == CloseCode::Normal)
    );
    assert_eq!(calls.writes.load(Ordering::SeqCst), 0);
    assert_eq!(calls.finalizes.load(Ordering::SeqCst), 1);
    assert_eq!(calls.closes.load(Ordering::SeqCst), 1);
    socket.flush().await.unwrap();
    drop(socket);
    gateway.stop().await;
}
