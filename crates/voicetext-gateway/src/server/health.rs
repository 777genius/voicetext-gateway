//! Minimal liveness/readiness endpoints and production dependency probe.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;
use voicetext_audio::discord_opus::DiscordOpusDecoder;
use voicetext_speech::application::ports::BoxFuture;

use crate::contracts::batch::BatchIdentity;
use crate::contracts::live::LiveIdentity;
use crate::profiles::ProfileRegistry;

use super::state::{GatewayReadiness, GatewayState, ReadinessFailure};

/// Readiness probe for the production `PostgreSQL` and durable-spool composition.
#[derive(Clone, Debug)]
pub struct PostgresSpoolReadiness {
    pool: PgPool,
    spool_directory: PathBuf,
}

impl PostgresSpoolReadiness {
    /// Binds the already-migrated pool and configured spool directory.
    #[must_use]
    pub fn new(pool: PgPool, spool_directory: PathBuf) -> Self {
        Self {
            pool,
            spool_directory,
        }
    }
}

impl GatewayReadiness for PostgresSpoolReadiness {
    fn check(&self) -> BoxFuture<'_, Result<(), ReadinessFailure>> {
        Box::pin(async move {
            let value = sqlx::query_scalar::<_, i32>("SELECT 1")
                .fetch_one(&self.pool)
                .await
                .map_err(|_| ReadinessFailure::new("DATABASE_UNAVAILABLE"))?;
            if value != 1 {
                return Err(ReadinessFailure::new("DATABASE_INVALID_PROBE"));
            }
            let directory = self.spool_directory.clone();
            tokio::task::spawn_blocking(move || probe_spool(&directory))
                .await
                .map_err(|_| ReadinessFailure::new("SPOOL_PROBE_TASK_FAILED"))??;
            if !DiscordOpusDecoder::runtime_available() {
                return Err(ReadinessFailure::new("OPUS_RUNTIME_UNAVAILABLE"));
            }
            Ok(())
        })
    }
}

fn probe_spool(directory: &Path) -> Result<(), ReadinessFailure> {
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|_| ReadinessFailure::new("SPOOL_UNAVAILABLE"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ReadinessFailure::new("SPOOL_INVALID_ROOT"));
    }

    let path = directory.join(format!(
        ".voicetext-readiness-{}",
        Uuid::new_v4().hyphenated()
    ));
    let mut probe = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| ReadinessFailure::new("SPOOL_NOT_WRITABLE"))?;
    let result = (|| {
        probe
            .write_all(b"ready")
            .and_then(|()| probe.sync_all())
            .map_err(|_| ReadinessFailure::new("SPOOL_WRITE_FAILED"))?;
        std::fs::remove_file(&path).map_err(|_| ReadinessFailure::new("SPOOL_CLEANUP_FAILED"))?;
        File::open(directory)
            .and_then(|root| root.sync_all())
            .map_err(|_| ReadinessFailure::new("SPOOL_SYNC_FAILED"))
    })();
    if result.is_err() {
        let _cleanup = std::fs::remove_file(path);
    }
    result
}

#[derive(Debug, Serialize)]
pub(crate) struct HealthBody {
    status: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct CompatibilityHealthBody {
    status: &'static str,
    provider_profiles: Vec<CompatibilityProfile>,
}

#[derive(Debug, Serialize)]
struct CompatibilityProfile {
    mode: &'static str,
    model: &'static str,
    profile: &'static str,
    provider: &'static str,
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    contract_version: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_version: Option<u8>,
}

pub(crate) async fn live() -> Json<HealthBody> {
    Json(HealthBody { status: "alive" })
}

pub(crate) async fn ready(State(state): State<GatewayState>) -> Response {
    if dependencies_ready(&state).await {
        return (StatusCode::OK, Json(HealthBody { status: "ready" })).into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(HealthBody {
            status: "not_ready",
        }),
    )
        .into_response()
}

pub(crate) async fn compatibility(State(state): State<GatewayState>) -> Response {
    let is_ready = dependencies_ready(&state).await;
    let status = if is_ready { "ok" } else { "not_ready" };
    let status_code = if is_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status_code,
        Json(compatibility_body(state.profiles(), status, is_ready)),
    )
        .into_response()
}

async fn dependencies_ready(state: &GatewayState) -> bool {
    if !state.startup_reconciled() || !state.accepting_work() {
        return false;
    }
    match state.readiness().check().await {
        Ok(()) => state.accepting_work(),
        Err(failure) => {
            tracing::warn!(code = failure.code(), "gateway readiness failed");
            false
        }
    }
}

fn compatibility_body(
    profiles: &ProfileRegistry,
    status: &'static str,
    dependencies_ready: bool,
) -> CompatibilityHealthBody {
    let deepgram_batch = BatchIdentity::DeepgramNova3MultiV2;
    let elevenlabs_batch = BatchIdentity::ElevenlabsScribeV2MultiV3;
    let deepgram_live = LiveIdentity::DeepgramNova3;
    let elevenlabs_live = LiveIdentity::ElevenlabsScribeV2Realtime;
    CompatibilityHealthBody {
        status,
        provider_profiles: vec![
            live_profile(
                "deepgram-nova-3",
                deepgram_live,
                dependencies_ready && profiles.live(deepgram_live).is_some(),
            ),
            live_profile(
                "elevenlabs-scribe-v2-realtime",
                elevenlabs_live,
                dependencies_ready && profiles.live(elevenlabs_live).is_some(),
            ),
            batch_profile(
                "deepgram-nova-3",
                deepgram_batch,
                dependencies_ready && profiles.batch(deepgram_batch).is_some(),
            ),
            batch_profile(
                "elevenlabs-scribe-v2",
                elevenlabs_batch,
                dependencies_ready && profiles.batch(elevenlabs_batch).is_some(),
            ),
        ],
    }
}

fn live_profile(
    profile: &'static str,
    identity: LiveIdentity,
    ready: bool,
) -> CompatibilityProfile {
    let (provider, model) = match identity {
        LiveIdentity::DeepgramNova3 => ("deepgram", "nova-3"),
        LiveIdentity::ElevenlabsScribeV2Realtime => ("elevenlabs", "scribe_v2_realtime"),
    };
    CompatibilityProfile {
        mode: "live",
        model,
        profile,
        provider,
        ready,
        contract_version: None,
        protocol_version: Some(2),
    }
}

fn batch_profile(
    profile: &'static str,
    identity: BatchIdentity,
    ready: bool,
) -> CompatibilityProfile {
    CompatibilityProfile {
        mode: "batch",
        model: identity.model(),
        profile,
        provider: identity.provider(),
        ready,
        contract_version: Some(identity.contract_version()),
        protocol_version: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn spool_probe_proves_write_and_cleans_up() {
        let directory = tempdir().unwrap();
        probe_spool(directory.path()).unwrap();
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn compatibility_projection_is_stable_and_fail_closed() {
        let body = compatibility_body(&ProfileRegistry::new(), "ok", true);
        let value = serde_json::to_value(body).unwrap();

        assert_eq!(
            value,
            json!({
                "status": "ok",
                "provider_profiles": [
                    {
                        "mode": "live",
                        "model": "nova-3",
                        "profile": "deepgram-nova-3",
                        "protocol_version": 2,
                        "provider": "deepgram",
                        "ready": false
                    },
                    {
                        "mode": "live",
                        "model": "scribe_v2_realtime",
                        "profile": "elevenlabs-scribe-v2-realtime",
                        "protocol_version": 2,
                        "provider": "elevenlabs",
                        "ready": false
                    },
                    {
                        "contract_version": 2,
                        "mode": "batch",
                        "model": "nova-3",
                        "profile": "deepgram-nova-3",
                        "provider": "deepgram",
                        "ready": false
                    },
                    {
                        "contract_version": 3,
                        "mode": "batch",
                        "model": "scribe_v2",
                        "profile": "elevenlabs-scribe-v2",
                        "provider": "elevenlabs",
                        "ready": false
                    }
                ]
            })
        );
    }
}
