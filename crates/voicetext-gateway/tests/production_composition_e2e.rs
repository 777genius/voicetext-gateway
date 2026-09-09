mod provider_wire;
#[allow(dead_code, unused_imports)]
mod support;

use std::env;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::time::sleep;
use uuid::Uuid;
use voicetext_gateway::contracts::batch::BatchIdentity;
use voicetext_gateway::storage::{DurableFileSpool, PostgresBatchJobStore};
use voicetext_speech::application::batch::{BatchAdmissionRequest, BatchCoordinator};
use voicetext_speech::application::ports::{
    BatchRecognitionRequest, BatchRecognitionResult, BatchRecognizer, BoxFuture, RecognitionFailure,
};
use voicetext_speech::domain::batch::{BatchProfile, BatchRequestFingerprint};

use provider_wire::{DEEPGRAM_KEY, ELEVENLABS_KEY, RunningProviderWire};
use support::synthetic_ogg_opus;

const TOKEN: &str = "conformance-service-token-00000001";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VOICETEXT_TEST_DATABASE_URL, Node.js, and a built production binary"]
async fn production_binary_matches_the_typescript_consumer_through_real_provider_adapters() {
    let database_url = env::var("VOICETEXT_TEST_DATABASE_URL")
        .expect("VOICETEXT_TEST_DATABASE_URL must identify a disposable database");
    assert_disposable_database(&database_url);
    let consumer_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contract-fixtures/typescript-consumer");
    assert!(
        consumer_root
            .join("voicetext-gateway-contract.ts")
            .is_file(),
        "pinned TypeScript contract fixture is missing"
    );

    let wire = RunningProviderWire::start().await;
    let sandbox = tempfile::tempdir().unwrap();
    let fixture_path = sandbox.path().join("synthetic.ogg");
    fs::write(&fixture_path, synthetic_ogg_opus()).unwrap();
    let spool_path = sandbox.path().join("spool");
    fs::create_dir(&spool_path).unwrap();
    let token_path = write_secret(sandbox.path(), "gateway-token", TOKEN);
    let database_path = write_secret(sandbox.path(), "postgres-url", &database_url);
    let deepgram_path = write_secret(sandbox.path(), "deepgram-key", DEEPGRAM_KEY);
    let elevenlabs_path = write_secret(sandbox.path(), "elevenlabs-key", ELEVENLABS_KEY);
    let address = reserve_loopback_address();

    let mut gateway = GatewayProcess::start(&GatewayProcessConfig {
        address,
        database_path: &database_path,
        deepgram_path: &deepgram_path,
        elevenlabs_path: &elevenlabs_path,
        spool_path: &spool_path,
        token_path: &token_path,
        wire: &wire,
    });
    wait_until_ready(address, &mut gateway).await;

    let result = Command::new("node")
        .current_dir(&consumer_root)
        .arg("voicetext-gateway-contract.ts")
        .env(
            "VOICETEXT_GATEWAY_E2E_HTTP_ORIGIN",
            format!("http://{address}"),
        )
        .env("VOICETEXT_GATEWAY_E2E_WS_ORIGIN", format!("ws://{address}"))
        .env("VOICETEXT_GATEWAY_E2E_TOKEN", TOKEN)
        .env("VOICETEXT_GATEWAY_E2E_OGG_FIXTURE", &fixture_path)
        .env("VOICETEXT_GATEWAY_E2E_PROVIDER_WIRE", "true")
        .status()
        .expect("could not start the Discord TypeScript consumer");

    drop(gateway);
    let counters = wire.counters().snapshot();
    wire.stop().await;
    assert!(result.success(), "Discord TypeScript consumer failed");
    assert_eq!(
        counters,
        [2, 2, 2, 2],
        "each provider adapter must receive one original request and no replay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VOICETEXT_TEST_DATABASE_URL for a new disposable database"]
async fn production_restart_binds_while_multi_page_recovery_provider_is_blocked() {
    let database_url = env::var("VOICETEXT_TEST_DATABASE_URL")
        .expect("VOICETEXT_TEST_DATABASE_URL must identify a disposable database");
    assert_disposable_database(&database_url);
    let sandbox = tempfile::tempdir().unwrap();
    let spool_path = sandbox.path().join("spool");
    fs::create_dir(&spool_path).unwrap();
    seed_recovery_backlog(&database_url, &spool_path, 101).await;

    let wire = RunningProviderWire::start_with_batch_blocked(true).await;
    let token_path = write_secret(sandbox.path(), "gateway-token", TOKEN);
    let database_path = write_secret(sandbox.path(), "postgres-url", &database_url);
    let deepgram_path = write_secret(sandbox.path(), "deepgram-key", DEEPGRAM_KEY);
    let elevenlabs_path = write_secret(sandbox.path(), "elevenlabs-key", ELEVENLABS_KEY);
    let address = reserve_loopback_address();
    let mut gateway = GatewayProcess::start(&GatewayProcessConfig {
        address,
        database_path: &database_path,
        deepgram_path: &deepgram_path,
        elevenlabs_path: &elevenlabs_path,
        spool_path: &spool_path,
        token_path: &token_path,
        wire: &wire,
    });

    wait_until_ready(address, &mut gateway).await;
    wait_for_batch_effect(&wire, &mut gateway).await;
    let metrics = reqwest::get(format!("http://{address}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("voicetext_batch_provider_effects_total 1"));
    assert!(metrics.contains("voicetext_batch_provider_effects_persisted_total 0"));

    drop(gateway);
    wire.release_batch();
    wire.stop().await;
}

#[derive(Debug)]
struct AdmissionOnlyRecognizer;

impl BatchRecognizer for AdmissionOnlyRecognizer {
    fn capabilities(
        &self,
    ) -> &'static voicetext_speech::application::batch_capabilities::BatchCapabilityDescriptor {
        support::batch_capabilities(BatchIdentity::DeepgramNova3MultiV2)
    }

    fn recognize(
        &self,
        _: BatchRecognitionRequest,
    ) -> BoxFuture<'_, Result<BatchRecognitionResult, RecognitionFailure>> {
        Box::pin(async { panic!("backlog seeding never executes providers") })
    }
}

async fn seed_recovery_backlog(database_url: &str, spool_path: &Path, count: u8) {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .unwrap();
    PostgresBatchJobStore::migrate(&pool).await.unwrap();
    let jobs = PostgresBatchJobStore::new(pool.clone());
    let spool = DurableFileSpool::new(spool_path, 1_048_576).unwrap();
    let recognizer = AdmissionOnlyRecognizer;
    let coordinator = BatchCoordinator::new(&recognizer, &jobs, &spool);
    for index in 1..=count {
        let id = Uuid::from_u128(u128::from(index)).hyphenated().to_string();
        coordinator
            .admit(BatchAdmissionRequest {
                id: voicetext_speech::application::ports::BatchJobId::new(id),
                profile: BatchProfile::new(2, "deepgram", "nova-3", "multi").unwrap(),
                fingerprint: BatchRequestFingerprint::from_bytes([index; 32]),
                audio: vec![index; 32],
                authoritative_duration_millis: 20,
                keyterms: Vec::new(),
            })
            .await
            .unwrap();
    }
    pool.close().await;
}

async fn wait_for_batch_effect(wire: &RunningProviderWire, gateway: &mut GatewayProcess) {
    for _ in 0..100 {
        gateway.assert_running();
        if wire.counters().snapshot()[0] == 1 {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("recovery worker did not reach the blocked local provider");
}

struct GatewayProcessConfig<'a> {
    address: SocketAddr,
    database_path: &'a Path,
    deepgram_path: &'a Path,
    elevenlabs_path: &'a Path,
    spool_path: &'a Path,
    token_path: &'a Path,
    wire: &'a RunningProviderWire,
}

struct GatewayProcess(Child);

impl GatewayProcess {
    fn start(config: &GatewayProcessConfig<'_>) -> Self {
        let binary = env::var_os("VOICETEXT_GATEWAY_PRODUCTION_BINARY").map_or_else(
            || PathBuf::from(env!("CARGO_BIN_EXE_voicetext-gateway")),
            PathBuf::from,
        );
        let child = Command::new(binary)
            .env_clear()
            .env("RUST_LOG", "error")
            .env("VOICETEXT_ALLOW_INSECURE_PROVIDER_ENDPOINTS", "true")
            .env("VOICETEXT_BEARER_TOKEN_FILE", config.token_path)
            .env("VOICETEXT_BIND_ADDR", config.address.to_string())
            .env("VOICETEXT_DEEPGRAM_API_KEY_FILE", config.deepgram_path)
            .env(
                "VOICETEXT_DEEPGRAM_BATCH_ENDPOINT",
                config.wire.deepgram_batch_endpoint(),
            )
            .env(
                "VOICETEXT_DEEPGRAM_LIVE_ENDPOINT",
                config.wire.deepgram_live_endpoint(),
            )
            .env("VOICETEXT_ELEVENLABS_API_KEY_FILE", config.elevenlabs_path)
            .env(
                "VOICETEXT_ELEVENLABS_BATCH_ENDPOINT",
                config.wire.elevenlabs_batch_endpoint(),
            )
            .env(
                "VOICETEXT_ELEVENLABS_LIVE_ENDPOINT",
                config.wire.elevenlabs_live_endpoint(),
            )
            .env("VOICETEXT_FINALIZE_TIMEOUT_MS", "3000")
            .env("VOICETEXT_MAX_CONNECTIONS", "8")
            .env("VOICETEXT_MAX_UPLOAD_BYTES", "1048576")
            .env("VOICETEXT_POSTGRES_URL_FILE", config.database_path)
            .env("VOICETEXT_SPOOL_DIR", config.spool_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("could not start the production gateway binary");
        Self(child)
    }

    fn assert_running(&mut self) {
        assert!(
            self.0.try_wait().unwrap().is_none(),
            "production gateway exited before readiness"
        );
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        let _kill = self.0.kill();
        let _wait = self.0.wait();
    }
}

async fn wait_until_ready(address: SocketAddr, gateway: &mut GatewayProcess) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(250))
        .build()
        .unwrap();
    for _attempt in 0..100 {
        gateway.assert_running();
        if client
            .get(format!("http://{address}/health/ready"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("production gateway did not become ready");
}

fn write_secret(directory: &Path, name: &str, value: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, value).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    }
    path
}

fn reserve_loopback_address() -> SocketAddr {
    let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).unwrap();
    listener.local_addr().unwrap()
}

fn assert_disposable_database(database_url: &str) {
    let options = PgConnectOptions::from_str(database_url).expect("valid PostgreSQL URL");
    assert!(
        options
            .get_database()
            .is_some_and(|database| database.starts_with("voicetext_test_")),
        "refusing non-disposable database"
    );
}
