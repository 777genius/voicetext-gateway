//! `VoiceText` Gateway process composition.

use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::oneshot;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;
use url::Url;
use voicetext_gateway::config::GatewayConfig;
use voicetext_gateway::profiles::ProfileRegistry;
use voicetext_gateway::server::{
    FileQualificationSink, GatewayLimits, GatewayState, PostgresSpoolReadiness, reconcile_startup,
    router, start_startup_recovery,
};
use voicetext_gateway::storage::{DurableFileSpool, PostgresBatchJobStore};

const SPOOL_ORPHAN_RETENTION: Duration = Duration::from_hours(24);
use voicetext_providers::deepgram::{DeepgramBatchRecognizer, DeepgramLiveRecognizer};
use voicetext_providers::elevenlabs::{ElevenLabsBatchRecognizer, ElevenLabsLiveRecognizer};

mod inherited_fd;

fn main() -> ExitCode {
    let health = std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("healthcheck"));
    let startup = if health {
        None
    } else {
        let Ok(config) = GatewayConfig::from_env() else {
            eprintln!("{}", BootstrapFailure::Configuration.code());
            return ExitCode::FAILURE;
        };
        let Ok(captured) = inherited_fd::Captured::capture(&config) else {
            eprintln!("SECRET_FD_INVALID");
            return ExitCode::FAILURE;
        };
        Some((config, captured))
    };
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::FAILURE;
    };
    runtime.block_on(async_main(startup))
}

async fn async_main(startup: Option<(GatewayConfig, inherited_fd::Captured)>) -> ExitCode {
    initialize_tracing();
    if install_crypto_provider().is_err() {
        tracing::error!(
            code = BootstrapFailure::CryptoProvider.code(),
            "gateway terminated"
        );
        return ExitCode::FAILURE;
    }
    if startup.is_none() {
        return if healthcheck().await {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let (config, captured) = startup.expect("normal startup captured before runtime");
    match run(config, captured).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(code = error.code(), "gateway terminated");
            ExitCode::FAILURE
        }
    }
}

fn install_crypto_provider() -> Result<(), BootstrapFailure> {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return Ok(());
    }
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| BootstrapFailure::CryptoProvider)
}

async fn healthcheck() -> bool {
    let Ok(client) = Client::builder().timeout(Duration::from_secs(2)).build() else {
        return false;
    };
    client
        .get("http://127.0.0.1:8080/health/ready")
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

async fn run(
    config: GatewayConfig,
    captured: inherited_fd::Captured,
) -> Result<(), BootstrapFailure> {
    let auth = captured
        .machine(&config.bearer_token_file)
        .await
        .map_err(|()| BootstrapFailure::BearerSecret)?;
    let database_url = captured
        .text(&config.postgres_url_file)
        .await
        .map_err(|()| BootstrapFailure::DatabaseSecret)?;
    let pool = database_pool_options(&config)
        .connect(database_url.expose_secret())
        .await
        .map_err(|_| BootstrapFailure::DatabaseConnect)?;
    drop(database_url);
    PostgresBatchJobStore::migrate(&pool)
        .await
        .map_err(|_| BootstrapFailure::DatabaseMigration)?;

    let spool = Arc::new(
        DurableFileSpool::new(&config.spool_directory, config.max_upload_bytes)
            .map_err(|_| BootstrapFailure::Spool)?,
    );
    let profiles = build_profiles(&config, &captured).await?;
    drop(captured);
    if !profiles.is_operational() {
        return Err(BootstrapFailure::NoProvider);
    }
    let connections =
        NonZeroUsize::new(config.max_connections).ok_or(BootstrapFailure::TransportLimits)?;
    let limits = GatewayLimits::new(
        config.max_upload_bytes,
        64 * 1_024,
        connections,
        Duration::from_secs(15),
        config.finalize_timeout,
    )
    .map_err(|_| BootstrapFailure::TransportLimits)?;
    let jobs = Arc::new(PostgresBatchJobStore::new(pool.clone()));
    let readiness = Arc::new(PostgresSpoolReadiness::new(
        pool.clone(),
        config.spool_directory.clone(),
    ));
    let mut state = GatewayState::new(
        auth,
        jobs.clone(),
        spool.clone(),
        profiles,
        readiness,
        limits,
    );
    if let Some(qualification) = &config.qualification_observation {
        let sink = Arc::new(
            FileQualificationSink::new(&qualification.directory, &qualification.campaign)
                .map_err(|_| BootstrapFailure::QualificationObservation)?,
        );
        state = state.with_qualification_observers(sink.clone(), sink);
    }

    let recovery = reconcile_startup(&state)
        .await
        .map_err(|_| BootstrapFailure::Recovery)?;
    let maintenance = spool
        .reconcile(jobs.as_ref(), SPOOL_ORPHAN_RETENTION)
        .await
        .map_err(|_| BootstrapFailure::Spool)?;
    state.record_startup_metrics(
        0,
        recovery.summary.recovered_unknown,
        maintenance.terminal_removed,
        maintenance.orphan_removed,
        maintenance.used_bytes,
        maintenance.capacity_bytes,
    );
    tracing::info!(
        pages = recovery.summary.pages,
        recovered_unknown = recovery.summary.recovered_unknown,
        terminal_audio_removed = maintenance.terminal_removed,
        orphan_audio_removed = maintenance.orphan_removed,
        spool_used_bytes = maintenance.used_bytes,
        spool_capacity_bytes = maintenance.capacity_bytes,
        "batch startup reconciliation completed"
    );

    let listener = tokio::net::TcpListener::bind(config.bind_address)
        .await
        .map_err(|_| BootstrapFailure::Bind)?;
    log_listening(config.bind_address);
    start_startup_recovery(&state, recovery);
    serve_until_shutdown(listener, state, pool, config.shutdown_drain_timeout).await
}

fn database_pool_options(config: &GatewayConfig) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
}

async fn serve_until_shutdown(
    listener: tokio::net::TcpListener,
    state: GatewayState,
    pool: PgPool,
    shutdown_drain_timeout: Duration,
) -> Result<(), BootstrapFailure> {
    let (stop_server, server_shutdown) = oneshot::channel();
    let server_state = state.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, router(server_state))
            .with_graceful_shutdown(async move {
                let _signal = server_shutdown.await;
            })
            .await
    });
    let shutdown_state = state.clone();
    tokio::select! {
        result = &mut server => {
            pool.close().await;
            return result.map_err(|_| BootstrapFailure::Serve)?
                .map_err(|_| BootstrapFailure::Serve);
        }
        () = after_shutdown_signal(wait_for_process_signal(), move || shutdown_state.begin_shutdown()) => {}
    }
    let _notified = stop_server.send(());
    let shutdown_deadline = tokio::time::Instant::now() + shutdown_drain_timeout;
    let aborted = state.shutdown_batch_tasks(shutdown_deadline).await;
    if aborted != 0 {
        tracing::warn!(aborted, "batch shutdown drain deadline reached");
    }
    let serve_result = match tokio::time::timeout_at(shutdown_deadline, &mut server).await {
        Ok(Ok(result)) => result.map_err(|_| BootstrapFailure::Serve),
        Ok(Err(_)) => Err(BootstrapFailure::Serve),
        Err(_) => {
            tracing::warn!("server graceful shutdown deadline reached");
            server.abort();
            let _joined = server.await;
            Ok(())
        }
    };
    pool.close().await;
    serve_result
}

async fn build_profiles(
    config: &GatewayConfig,
    captured: &inherited_fd::Captured,
) -> Result<ProfileRegistry, BootstrapFailure> {
    let client = provider_http_client(config.allow_insecure_provider_endpoints)?;
    let endpoints = &config.provider_endpoints;
    let mut profiles = ProfileRegistry::new();

    if let Some(path) = &config.deepgram_api_key_file {
        let key = captured
            .text(path)
            .await
            .map_err(|()| BootstrapFailure::ProviderSecret)?;
        let batch = DeepgramBatchRecognizer::new(
            client.clone(),
            key.expose_secret(),
            parse_url(&endpoints.deepgram_batch)?,
        )
        .map_err(|_| BootstrapFailure::ProviderConfiguration)?;
        let live =
            DeepgramLiveRecognizer::new(key.expose_secret(), parse_url(&endpoints.deepgram_live)?)
                .map_err(|_| BootstrapFailure::ProviderConfiguration)?;
        profiles = profiles
            .with_batch(Arc::new(batch))
            .with_live(Arc::new(live));
    }
    if let Some(path) = &config.elevenlabs_api_key_file {
        let key = captured
            .text(path)
            .await
            .map_err(|()| BootstrapFailure::ProviderSecret)?;
        let batch = ElevenLabsBatchRecognizer::new(
            client,
            key.expose_secret(),
            parse_url(&endpoints.elevenlabs_batch)?,
        )
        .map_err(|_| BootstrapFailure::ProviderConfiguration)?;
        let live = ElevenLabsLiveRecognizer::new(
            key.expose_secret(),
            parse_url(&endpoints.elevenlabs_live)?,
        )
        .map_err(|_| BootstrapFailure::ProviderConfiguration)?;
        profiles = profiles
            .with_batch(Arc::new(batch))
            .with_live(Arc::new(live));
    }
    Ok(profiles)
}

fn parse_url(value: &str) -> Result<Url, BootstrapFailure> {
    Url::parse(value).map_err(|_| BootstrapFailure::ProviderConfiguration)
}

fn provider_http_client(allow_insecure: bool) -> Result<Client, BootstrapFailure> {
    Client::builder()
        .https_only(!allow_insecure)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_mins(3))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("voicetext-gateway/0.1")
        .build()
        .map_err(|_| BootstrapFailure::ProviderConfiguration)
}

fn initialize_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _initialized = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json().with_target(false))
        .try_init();
}

fn log_listening(address: SocketAddr) {
    tracing::info!(%address, "gateway listening");
}

async fn wait_for_process_signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    if let Ok(mut terminate) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! {
            _ = interrupt => {}
            _ = terminate.recv() => {}
        }
        return;
    }
    let _signal = interrupt.await;
}

async fn after_shutdown_signal(signal: impl Future<Output = ()>, stop_admission: impl FnOnce()) {
    signal.await;
    stop_admission();
}

#[derive(Clone, Copy, Debug)]
enum BootstrapFailure {
    CryptoProvider,
    Configuration,
    BearerSecret,
    DatabaseSecret,
    DatabaseConnect,
    DatabaseMigration,
    Recovery,
    Spool,
    ProviderSecret,
    ProviderConfiguration,
    QualificationObservation,
    NoProvider,
    TransportLimits,
    Bind,
    Serve,
}

impl BootstrapFailure {
    const fn code(self) -> &'static str {
        match self {
            Self::CryptoProvider => "CRYPTO_PROVIDER_INSTALL_FAILED",
            Self::Configuration => "CONFIGURATION_INVALID",
            Self::BearerSecret => "BEARER_SECRET_INVALID",
            Self::DatabaseSecret => "DATABASE_SECRET_INVALID",
            Self::DatabaseConnect => "DATABASE_CONNECT_FAILED",
            Self::DatabaseMigration => "DATABASE_MIGRATION_FAILED",
            Self::Recovery => "BATCH_RECOVERY_FAILED",
            Self::Spool => "SPOOL_INVALID",
            Self::ProviderSecret => "PROVIDER_SECRET_INVALID",
            Self::ProviderConfiguration => "PROVIDER_CONFIGURATION_INVALID",
            Self::QualificationObservation => "QUALIFICATION_OBSERVATION_INVALID",
            Self::NoProvider => "NO_PROVIDER_CONFIGURED",
            Self::TransportLimits => "TRANSPORT_LIMITS_INVALID",
            Self::Bind => "LISTENER_BIND_FAILED",
            Self::Serve => "SERVER_FAILED",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::sync::oneshot;

    use super::{after_shutdown_signal, install_crypto_provider};

    #[test]
    fn crypto_provider_installation_is_idempotent() {
        assert!(install_crypto_provider().is_ok());
        assert!(install_crypto_provider().is_ok());
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[tokio::test]
    async fn sigterm_transition_stops_admission_only_after_the_signal() {
        let stopped = Arc::new(AtomicBool::new(false));
        let task_stopped = Arc::clone(&stopped);
        let (send_signal, receive_signal) = oneshot::channel();
        let transition = tokio::spawn(after_shutdown_signal(
            async move {
                let _signal = receive_signal.await;
            },
            move || task_stopped.store(true, Ordering::SeqCst),
        ));
        tokio::task::yield_now().await;
        assert!(!stopped.load(Ordering::SeqCst));
        send_signal.send(()).unwrap();
        transition.await.unwrap();
        assert!(stopped.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod database_pool_composition {
    use super::*;
    use voicetext_gateway::config::{
        BEARER_TOKEN_FILE_ENV, DATABASE_MAX_CONNECTIONS_ENV, POSTGRES_URL_FILE_ENV,
        SPOOL_DIRECTORY_ENV,
    };

    fn config(limit: Option<&str>) -> GatewayConfig {
        GatewayConfig::from_lookup(|name| {
            match name {
                POSTGRES_URL_FILE_ENV => Some("/unused/database"),
                BEARER_TOKEN_FILE_ENV => Some("/unused/bearer"),
                SPOOL_DIRECTORY_ENV => Some("/unused/spool"),
                DATABASE_MAX_CONNECTIONS_ENV => limit,
                _ => None,
            }
            .map(str::to_owned)
        })
        .unwrap()
    }

    #[test]
    fn database_pool_composes_validated_limit_and_preserves_timeouts() {
        for (limit, expected) in [(None, 10), (Some("1"), 1), (Some("7"), 7)] {
            let options = database_pool_options(&config(limit));
            assert_eq!(options.get_max_connections(), expected);
            assert_eq!(options.get_min_connections(), 1);
            assert_eq!(options.get_acquire_timeout(), Duration::from_secs(10));
        }
    }

    #[tokio::test]
    #[ignore = "requires a disposable local PostgreSQL database and non-superuser role, both CONNECTION LIMIT 1"]
    async fn constrained_database_serializes_poll_and_execution_acquisition() {
        use sqlx::postgres::PgConnectOptions;
        use std::str::FromStr;

        let url = std::env::var("VOICETEXT_TEST_DATABASE_URL")
            .expect("set VOICETEXT_TEST_DATABASE_URL to a disposable local database");
        let options = PgConnectOptions::from_str(&url).expect("valid database URL");
        assert!(matches!(
            options.get_host(),
            "localhost" | "127.0.0.1" | "::1"
        ));
        assert!(
            options
                .get_database()
                .unwrap()
                .starts_with("voicetext_test_")
        );
        let pool = database_pool_options(&config(Some("1")))
            .connect_with(options)
            .await
            .unwrap();
        let constraints: (bool, i32, i32) = sqlx::query_as(
            "SELECT r.rolsuper, r.rolconnlimit, d.datconnlimit FROM pg_roles r, pg_database d \
             WHERE r.rolname = current_user AND d.datname = current_database()",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            constraints,
            (false, 1, 1),
            "fixture must enforce both connection limits"
        );

        // Hold the polling connection while the execution acquisition is actually polled.
        // With the previous maximum of 10, this tries a second connection and receives
        // PostgreSQL SQLSTATE 53300 instead of waiting for the polling lease.
        let mut held_lease = pool.acquire().await.unwrap();
        let poll_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *held_lease)
            .await
            .unwrap();
        let execution = pool.acquire();
        tokio::pin!(execution);
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut execution)
                .await
                .is_err()
        );
        drop(held_lease);
        let mut execution = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .unwrap()
            .unwrap();
        let execution_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *execution)
            .await
            .unwrap();
        assert_eq!(execution_pid, poll_pid);
        assert_eq!(pool.size(), 1);
        drop(execution);
        pool.close().await;
    }
}
