//! `VoiceText` Gateway process composition.

use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::Path;
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
use voicetext_gateway::secret::{MachineSecret, SecretText};
use voicetext_gateway::server::{
    FileQualificationSink, GatewayLimits, GatewayState, PostgresSpoolReadiness, reconcile_startup,
    router, start_startup_recovery,
};
use voicetext_gateway::storage::{DurableFileSpool, PostgresBatchJobStore};

const SPOOL_ORPHAN_RETENTION: Duration = Duration::from_hours(24);
use voicetext_providers::deepgram::{DeepgramBatchRecognizer, DeepgramLiveRecognizer};
use voicetext_providers::elevenlabs::{ElevenLabsBatchRecognizer, ElevenLabsLiveRecognizer};

#[tokio::main]
async fn main() -> ExitCode {
    initialize_tracing();
    if install_crypto_provider().is_err() {
        tracing::error!(
            code = BootstrapFailure::CryptoProvider.code(),
            "gateway terminated"
        );
        return ExitCode::FAILURE;
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("healthcheck")) {
        return if healthcheck().await {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    match run().await {
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

async fn run() -> Result<(), BootstrapFailure> {
    let config = GatewayConfig::from_env().map_err(|_| BootstrapFailure::Configuration)?;
    let auth = MachineSecret::read_from_file(&config.bearer_token_file)
        .await
        .map_err(|_| BootstrapFailure::BearerSecret)?;
    let database_url = SecretText::read_from_file(&config.postgres_url_file)
        .await
        .map_err(|_| BootstrapFailure::DatabaseSecret)?;
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
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
    let profiles = build_profiles(&config).await?;
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

async fn build_profiles(config: &GatewayConfig) -> Result<ProfileRegistry, BootstrapFailure> {
    let client = provider_http_client(config.allow_insecure_provider_endpoints)?;
    let endpoints = &config.provider_endpoints;
    let mut profiles = ProfileRegistry::new();

    if let Some(path) = &config.deepgram_api_key_file {
        let key = provider_secret(path).await?;
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
        let key = provider_secret(path).await?;
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

async fn provider_secret(path: &Path) -> Result<SecretText, BootstrapFailure> {
    SecretText::read_from_file(path)
        .await
        .map_err(|_| BootstrapFailure::ProviderSecret)
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
