mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use voicetext_gateway::server::{
    BatchObservation, BatchObservationSink, LiveObservation, LiveObservationSink,
    ObservationFuture, ObservationSinkFailure,
};

use support::{TOKEN, TestGateway, synthetic_ogg_opus};

const IDEMPOTENCY_KEY: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

#[derive(Debug, Default)]
struct CapturingSink {
    batch: Mutex<Vec<BatchObservation>>,
    live: Mutex<Vec<LiveObservation>>,
}

impl BatchObservationSink for CapturingSink {
    fn enabled(&self) -> bool {
        true
    }

    fn observe_batch(&self, record: BatchObservation) -> ObservationFuture<'_> {
        self.batch.lock().unwrap().push(record);
        Box::pin(async { Ok(()) })
    }
}

impl LiveObservationSink for CapturingSink {
    fn enabled(&self) -> bool {
        true
    }

    fn observe_live(&self, record: LiveObservation) -> ObservationFuture<'_> {
        self.live.lock().unwrap().push(record);
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug)]
struct FailingSink;

impl BatchObservationSink for FailingSink {
    fn enabled(&self) -> bool {
        true
    }

    fn observe_batch(&self, _record: BatchObservation) -> ObservationFuture<'_> {
        Box::pin(async { Err(ObservationSinkFailure("SYNTHETIC_OBSERVATION_FAILURE")) })
    }
}

impl LiveObservationSink for FailingSink {
    fn enabled(&self) -> bool {
        true
    }

    fn observe_live(&self, _record: LiveObservation) -> ObservationFuture<'_> {
        Box::pin(async { Err(ObservationSinkFailure("SYNTHETIC_OBSERVATION_FAILURE")) })
    }
}

#[tokio::test]
async fn batch_observation_binds_persisted_effect_and_replay_adds_nothing() {
    let sink = Arc::new(CapturingSink::default());
    let gateway = TestGateway::start_with_observers(sink.clone(), sink.clone()).await;
    let client = reqwest::Client::new();

    let accepted = post_batch(&client, &gateway).await;
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    let pending: Value = accepted.json().await.unwrap();
    let job_id = pending["job_id"].as_str().unwrap();
    let completed = poll_completed(&client, &gateway, job_id).await;
    assert_eq!(completed["status"], "completed");
    wait_for_count(&sink.batch, 1).await;

    let replay = post_batch(&client, &gateway).await;
    assert_eq!(replay.status(), StatusCode::OK);
    sleep(Duration::from_millis(50)).await;
    {
        let records = sink.batch.lock().unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.gateway_job_id, job_id);
        assert_eq!(record.terminal_status, "completed");
        assert_eq!(record.durable_persistence, "established");
        assert_eq!(record.profile.provider, "elevenlabs");
        assert_eq!(record.profile.model, "scribe_v2");
        assert_eq!(record.profile.contract_version, 3);
        assert_eq!(
            record.provider_operation.as_ref().unwrap().kind,
            "transcription_id"
        );
        assert_eq!(
            record.provider_operation.as_ref().unwrap().id,
            "fake-request-1"
        );
        assert!(record.result_digest.is_some());
        assert!(record.finished_at_unix_ms >= record.started_at_unix_ms);
    }
    gateway.stop().await;
}

#[tokio::test]
async fn default_composition_is_noop_and_exposes_no_observation_failures() {
    let gateway = TestGateway::start().await;
    assert!(gateway.http_origin().starts_with("http://127.0.0.1:"));
    assert!(gateway.websocket_origin().starts_with("ws://127.0.0.1:"));
    let metrics = reqwest::get(gateway.http_url("/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("voicetext_qualification_observation_failures_total 0\n"));
    gateway.stop().await;
}

#[tokio::test]
async fn sink_failure_is_observable_without_invalidating_authoritative_batch() {
    let sink = Arc::new(FailingSink);
    let gateway = TestGateway::start_with_observers(sink.clone(), sink).await;
    let client = reqwest::Client::new();
    let accepted = post_batch(&client, &gateway).await;
    let pending: Value = accepted.json().await.unwrap();
    let completed = poll_completed(&client, &gateway, pending["job_id"].as_str().unwrap()).await;
    assert_eq!(completed["status"], "completed");

    let metrics = client
        .get(gateway.http_url("/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("voicetext_qualification_observation_failures_total 1\n"));
    gateway.stop().await;
}

#[tokio::test]
async fn live_observation_separates_provider_write_from_delivered_ack_and_finalize() {
    let sink = Arc::new(CapturingSink::default());
    let gateway = TestGateway::start_with_observers(sink.clone(), sink.clone()).await;
    let mut request = gateway.websocket_url().into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap(),
    );
    let (mut socket, _) = connect_async(request).await.unwrap();
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
                "sample_rate": 16000
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let ready = next_json(&mut socket).await;
    assert_eq!(ready["type"], "ready");
    socket
        .send(Message::Binary(vec![1, 2].into()))
        .await
        .unwrap();
    loop {
        if next_json(&mut socket).await["type"] == "ack" {
            break;
        }
    }
    socket
        .send(Message::Text(json!({"type":"finalize"}).to_string().into()))
        .await
        .unwrap();
    loop {
        let message = next_json(&mut socket).await;
        if message["type"] == "finalize_complete" {
            assert_eq!(message["status"], "flushed");
            assert_eq!(message["saw_result"], true);
            break;
        }
    }
    wait_for_count(&sink.live, 1).await;
    {
        let records = sink.live.lock().unwrap();
        let record = &records[0];
        assert_eq!(record.accepted_frame_count, 1);
        assert_eq!(record.written_sequences.count, 1);
        assert_eq!(record.acked_sequences.count, 1);
        assert_eq!(record.written_sequences.first, Some(1));
        assert_eq!(record.acked_sequences.first, Some(1));
        assert!(record.finalize_result_observed);
        assert_eq!(record.terminal_status, "finalize_flushed");
        assert_eq!(
            record.provider_operation.as_ref().unwrap().kind,
            "request_id"
        );
        assert_eq!(record.gateway_session_id.to_string(), ready["session_id"]);
    }
    gateway.stop().await;
}

async fn post_batch(client: &reqwest::Client, gateway: &TestGateway) -> reqwest::Response {
    client
        .post(gateway.http_url("/api/v1/transcribe/batch"))
        .bearer_auth(TOKEN)
        .header("x-idempotency-key", IDEMPOTENCY_KEY)
        .multipart(
            Form::new()
                .text("contract_version", "3")
                .text("provider", "elevenlabs")
                .text("model", "scribe_v2")
                .text("language", "multi")
                .text("keyterms", "[]")
                .part(
                    "file",
                    Part::bytes(synthetic_ogg_opus())
                        .file_name("speaker-track.ogg")
                        .mime_str("audio/ogg")
                        .unwrap(),
                ),
        )
        .send()
        .await
        .unwrap()
}

async fn poll_completed(client: &reqwest::Client, gateway: &TestGateway, job_id: &str) -> Value {
    for _ in 0..50 {
        let response = client
            .get(gateway.http_url(&format!("/api/v1/transcribe/batch/{job_id}")))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.json::<Value>().await.unwrap();
        if status == StatusCode::OK {
            return body;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("batch job did not complete")
}

async fn wait_for_count<T>(records: &Mutex<Vec<T>>, expected: usize) {
    timeout(Duration::from_secs(2), async {
        loop {
            if records.lock().unwrap().len() == expected {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

async fn next_json<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let message = timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(text) = message else {
        panic!("expected text message")
    };
    serde_json::from_str(&text).unwrap()
}
