mod support;

use std::time::Duration;
use std::{env, fs, process::Command};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use uuid::Uuid;

use support::{TOKEN, TestGateway, synthetic_ogg_opus};

const IDEMPOTENCY_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ELEVENLABS_IDEMPOTENCY_KEY: &str =
    "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

#[tokio::test]
async fn current_voicetext_health_contract_is_preserved() {
    let gateway = TestGateway::start().await;
    let response = reqwest::get(gateway.http_url("/health")).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({
            "status": "ok",
            "provider_profiles": [
                {
                    "mode": "live",
                    "model": "nova-3",
                    "profile": "deepgram-nova-3",
                    "protocol_version": 2,
                    "provider": "deepgram",
                    "ready": true
                },
                {
                    "mode": "live",
                    "model": "scribe_v2_realtime",
                    "profile": "elevenlabs-scribe-v2-realtime",
                    "protocol_version": 2,
                    "provider": "elevenlabs",
                    "ready": true
                },
                {
                    "contract_version": 2,
                    "mode": "batch",
                    "model": "nova-3",
                    "profile": "deepgram-nova-3",
                    "provider": "deepgram",
                    "ready": true
                },
                {
                    "contract_version": 3,
                    "mode": "batch",
                    "model": "scribe_v2",
                    "profile": "elevenlabs-scribe-v2",
                    "provider": "elevenlabs",
                    "ready": true
                }
            ]
        })
    );
    gateway.stop().await;
}

#[tokio::test]
async fn exact_batch_v2_and_v3_wire_contracts() {
    let gateway = TestGateway::start().await;
    assert_batch_profile(
        &gateway,
        BatchProfile {
            version: "2",
            provider: "deepgram",
            model: "nova-3",
            idempotency_key: IDEMPOTENCY_KEY,
        },
    )
    .await;
    assert_batch_profile(
        &gateway,
        BatchProfile {
            version: "3",
            provider: "elevenlabs",
            model: "scribe_v2",
            idempotency_key: ELEVENLABS_IDEMPOTENCY_KEY,
        },
    )
    .await;
    gateway.stop().await;
}

#[tokio::test]
async fn exact_live_v2_ready_ack_final_and_finalize_contract() {
    let gateway = TestGateway::start().await;
    assert_live_profile(&gateway, "deepgram", "nova-3").await;
    assert_live_profile(&gateway, "elevenlabs", "scribe_v2_realtime").await;
    gateway.stop().await;
}

#[tokio::test]
async fn authentication_and_resource_bounds_fail_closed() {
    let gateway = TestGateway::start().await;
    let client = reqwest::Client::new();
    let unauthorized = client
        .get(gateway.http_url("/api/v1/transcribe/batch/123e4567-e89b-42d3-a456-426614174000"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauthorized.headers()["x-voicetext-error-code"],
        "UNAUTHORIZED"
    );
    let unauthorized_body = unauthorized.text().await.unwrap();
    assert_eq!(unauthorized_body, r#"{"error_code":"UNAUTHORIZED"}"#);
    assert!(!unauthorized_body.contains(TOKEN));

    let unauthenticated_websocket = connect_async(gateway.websocket_url()).await.unwrap_err();
    let WebSocketError::Http(response) = unauthenticated_websocket else {
        panic!("expected HTTP rejection before WebSocket upgrade");
    };
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let oversized = client
        .post(gateway.http_url("/api/v1/transcribe/batch"))
        .bearer_auth(TOKEN)
        .header("x-idempotency-key", "c".repeat(64))
        .multipart(batch_form_with_audio(
            BatchProfile {
                version: "2",
                provider: "deepgram",
                model: "nova-3",
                idempotency_key: "c",
            },
            &[],
            vec![0; 1024 * 1024 + 1],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        oversized.headers()["x-voicetext-error-code"],
        "MULTIPART_FIELD_TOO_LARGE"
    );

    assert_live_frame_bound(&gateway).await;
    let metrics = client
        .get(gateway.http_url("/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!metrics.contains(TOKEN));
    assert!(!metrics.contains('{'));
    gateway.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires DISCORD_MEETING_ASSISTANT_ROOT with installed pnpm dependencies"]
async fn checked_in_typescript_consumer_matches_the_real_gateway() {
    let consumer_root = env::var_os("DISCORD_MEETING_ASSISTANT_ROOT")
        .map(std::path::PathBuf::from)
        .expect("DISCORD_MEETING_ASSISTANT_ROOT is required");
    assert!(
        consumer_root
            .join("packages/voicetext-adapter/package.json")
            .is_file(),
        "Discord Meeting Assistant VoiceText adapter is missing"
    );
    let fixture_directory = tempfile::tempdir().unwrap();
    let fixture_path = fixture_directory.path().join("synthetic.ogg");
    fs::write(&fixture_path, synthetic_ogg_opus()).unwrap();
    let gateway = TestGateway::start().await;

    let result = Command::new("pnpm")
        .current_dir(&consumer_root)
        .args([
            "--filter",
            "@discord-meeting/voicetext-adapter",
            "exec",
            "vitest",
            "run",
            "test/voicetext-gateway-black-box.e2e.test.ts",
        ])
        .env("VOICETEXT_GATEWAY_E2E_HTTP_ORIGIN", gateway.http_origin())
        .env(
            "VOICETEXT_GATEWAY_E2E_WS_ORIGIN",
            gateway.websocket_origin(),
        )
        .env("VOICETEXT_GATEWAY_E2E_TOKEN", TOKEN)
        .env("VOICETEXT_GATEWAY_E2E_OGG_FIXTURE", &fixture_path)
        .status()
        .expect("could not start the Discord TypeScript conformance client");

    gateway.stop().await;
    assert!(
        result.success(),
        "Discord TypeScript conformance client failed"
    );
}

#[derive(Clone, Copy)]
struct BatchProfile {
    version: &'static str,
    provider: &'static str,
    model: &'static str,
    idempotency_key: &'static str,
}

async fn assert_batch_profile(gateway: &TestGateway, profile: BatchProfile) {
    let client = reqwest::Client::new();
    let response = client
        .post(gateway.http_url("/api/v1/transcribe/batch"))
        .bearer_auth(TOKEN)
        .header("x-idempotency-key", profile.idempotency_key)
        .multipart(batch_form(profile, &["Quanta"]))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let pending: Value = response.json().await.unwrap();
    let job_id = pending["job_id"].as_str().unwrap();
    Uuid::parse_str(job_id).unwrap();
    let expected_pending = if profile.version == "2" {
        json!({
            "success": true,
            "status": "running",
            "job_id": job_id,
            "next_action": "poll",
            "retry_after_ms": 1000
        })
    } else {
        json!({
            "contract_version": 3,
            "provider": profile.provider,
            "model": profile.model,
            "language": "multi",
            "success": true,
            "status": "running",
            "job_id": job_id,
            "next_action": "poll",
            "retry_after_ms": 1000
        })
    };
    assert_eq!(pending, expected_pending);

    let completed = poll_completed(&client, gateway, job_id).await;
    assert_eq!(completed, expected_completed(profile, job_id));

    let replay = client
        .post(gateway.http_url("/api/v1/transcribe/batch"))
        .bearer_auth(TOKEN)
        .header("x-idempotency-key", profile.idempotency_key)
        .multipart(batch_form(profile, &["Quanta"]))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(replay.json::<Value>().await.unwrap(), completed);

    let conflict = client
        .post(gateway.http_url("/api/v1/transcribe/batch"))
        .bearer_auth(TOKEN)
        .header("x-idempotency-key", profile.idempotency_key)
        .multipart(batch_form(profile, &["different"]))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
}

fn batch_form(profile: BatchProfile, keyterms: &[&str]) -> Form {
    batch_form_with_audio(profile, keyterms, synthetic_ogg_opus())
}

fn batch_form_with_audio(profile: BatchProfile, keyterms: &[&str], audio: Vec<u8>) -> Form {
    Form::new()
        .text("contract_version", profile.version)
        .text("provider", profile.provider)
        .text("model", profile.model)
        .text("language", "multi")
        .text("keyterms", serde_json::to_string(keyterms).unwrap())
        .part(
            "file",
            Part::bytes(audio)
                .file_name("speaker-track.ogg")
                .mime_str("audio/ogg")
                .unwrap(),
        )
}

async fn assert_live_frame_bound(gateway: &TestGateway) {
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
                "encoding": "opus",
                "sample_rate": 48000
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "ready");
    socket
        .send(Message::Binary(vec![0; 64 * 1024 + 1].into()))
        .await
        .unwrap();
    let bounded = timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("oversized live frame did not terminate the connection");
    if let Some(Ok(Message::Text(text))) = bounded {
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!({
                "type": "error",
                "code": "TRANSPORT_CLOSED",
                "message": "Live transport closed"
            })
        );
        let closed = timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("safe error was not followed by close");
        assert!(matches!(
            closed,
            None | Some(Err(_) | Ok(Message::Close(_)))
        ));
    } else {
        assert!(
            matches!(bounded, None | Some(Err(_) | Ok(Message::Close(_)))),
            "oversized live frame produced an unexpected message: {bounded:?}"
        );
    }
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
        let body: Value = response.json().await.unwrap();
        if status == StatusCode::OK {
            return body;
        }
        assert_eq!(status, StatusCode::ACCEPTED);
        sleep(Duration::from_millis(10)).await;
    }
    panic!("batch job did not complete");
}

fn expected_completed(profile: BatchProfile, job_id: &str) -> Value {
    if profile.version == "2" {
        json!({
            "success": true,
            "status": "completed",
            "job_id": job_id,
            "result": {
                "provider": "deepgram",
                "model": "nova-3",
                "language": "multi",
                "text": "synthetic speech",
                "duration_seconds": 0.014,
                "utterances": [{
                    "start": 0.0,
                    "end": 0.014,
                    "transcript": "synthetic speech",
                    "confidence": 0.949_999_988_079_071
                }],
                "readable_segments": [{
                    "start": 0.0,
                    "end": 0.014,
                    "transcript": "synthetic speech",
                    "source_utterance_indices": [0]
                }]
            }
        })
    } else {
        json!({
            "contract_version": 3,
            "provider": "elevenlabs",
            "model": "scribe_v2",
            "language": "multi",
            "success": true,
            "status": "completed",
            "job_id": job_id,
            "result": {
                "result_id": job_id,
                "provider": "elevenlabs",
                "model": "scribe_v2",
                "language": "multi",
                "text": "synthetic speech",
                "duration_ms": 14,
                "segments": [{
                    "index": 0,
                    "start_ms": 0,
                    "end_ms": 14,
                    "text": "synthetic speech",
                    "confidence": 0.949_999_988_079_071
                }],
                "provider_request": {"id": "fake-request-1"}
            }
        })
    }
}

async fn assert_live_profile(gateway: &TestGateway, provider: &str, model: &str) {
    let mut request = gateway.websocket_url().into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap(),
    );
    let (mut socket, response) = connect_async(request).await.unwrap();
    assert_eq!(response.status(), 101);
    let config = json!({
        "type": "config",
        "provider": provider,
        "model": model,
        "language": "multi",
        "capabilities": ["finalize_ack"],
        "channels": 1,
        "protocol_v": 2,
        "client_session_id": "123e4567-e89b-42d3-a456-426614174000",
        "encoding": "opus",
        "sample_rate": 48000,
        "keyterms": ["Quanta"]
    });
    socket
        .send(Message::Text(config.to_string().into()))
        .await
        .unwrap();

    let ready = next_json(&mut socket).await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["provider"], provider);
    assert_eq!(ready["model"], model);
    assert_eq!(ready.as_object().unwrap().len(), 4);
    Uuid::parse_str(ready["session_id"].as_str().unwrap()).unwrap();

    socket
        .send(Message::Binary(vec![0xf8, 0xff, 0xfe].into()))
        .await
        .unwrap();
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type": "ack", "seq": 1})
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({
            "type": "partial",
            "text": "synthetic live speech",
            "start_ms": 20,
            "duration_ms": 40,
            "confidence": 0.899_999_976_158_142_1
        })
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({
            "type": "final",
            "text": "synthetic live speech",
            "start_ms": 20,
            "duration_ms": 40,
            "confidence": 0.899_999_976_158_142_1
        })
    );

    socket
        .send(Message::Text(r#"{"type":"finalize"}"#.into()))
        .await
        .unwrap();
    assert_eq!(
        next_json(&mut socket).await,
        json!({
            "type": "finalize_complete",
            "status": "flushed",
            "saw_result": true
        })
    );
    socket.close(None).await.unwrap();
}

async fn next_json<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let message = timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("timed out waiting for gateway frame")
        .expect("gateway closed before expected frame")
        .unwrap();
    let Message::Text(text) = message else {
        panic!("expected gateway text frame, received {message:?}");
    };
    serde_json::from_str(&text).unwrap()
}
