#[path = "production_composition_e2e/provider_wire.rs"]
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

use sqlx::postgres::PgConnectOptions;
use tokio::time::sleep;

use provider_wire::{DEEPGRAM_KEY, ELEVENLABS_KEY, RunningProviderWire};
use support::synthetic_ogg_opus;

const TOKEN: &str = "conformance-service-token-00000001";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VOICETEXT_TEST_DATABASE_URL and DISCORD_MEETING_ASSISTANT_ROOT"]
async fn production_binary_matches_the_typescript_consumer_through_real_provider_adapters() {
    let database_url = env::var("VOICETEXT_TEST_DATABASE_URL")
        .expect("VOICETEXT_TEST_DATABASE_URL must identify a disposable database");
    assert_disposable_database(&database_url);
    let consumer_root = env::var_os("DISCORD_MEETING_ASSISTANT_ROOT")
        .map(PathBuf::from)
        .expect("DISCORD_MEETING_ASSISTANT_ROOT is required");
    assert!(
        consumer_root
            .join("packages/voicetext-adapter/package.json")
            .is_file(),
        "Discord Meeting Assistant VoiceText adapter is missing"
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
        let child = Command::new(env!("CARGO_BIN_EXE_voicetext-gateway"))
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
            .stderr(Stdio::null())
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
