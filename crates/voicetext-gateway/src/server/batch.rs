//! Exact VoiceText-compatible batch HTTP handlers.

use std::sync::Arc;

use axum::extract::{Multipart, Path, State, multipart::Field};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;
use voicetext_audio::ogg_opus::validate_complete_ogg_opus;
use voicetext_speech::application::batch::{
    BatchAdmissionOutcome, BatchAdmissionRequest, BatchCoordinator, BatchCoordinatorFailure,
};
use voicetext_speech::application::ports::{BatchJobId, BatchJobSnapshot};
use voicetext_speech::domain::batch::{BatchJobState, BatchProfile, BatchUnknownOutcome};

use super::error::GatewayHttpError;
use super::state::GatewayState;
use crate::auth::authenticate;
use crate::contracts::batch::{BatchIdentity, BatchResponseStatus, NextAction};
use crate::contracts::batch_outbound::{
    OutboundBatchResponse, OutboundReadableSegment, OutboundSegment, OutboundTranscription,
    serialize_response,
};
use crate::identity::{
    IdempotencyKey, canonicalize_keyterms, deterministic_job_id, request_fingerprint,
};

const IDEMPOTENCY_HEADER: &str = "x-idempotency-key";
const MAX_TEXT_FIELD_BYTES: usize = 24 * 1024;
const DEFAULT_POLL_MILLIS: u64 = 1_000;
const DEFAULT_RETRY_MILLIS: u64 = 1_000;

/// Accepts one exact, bounded `VoiceText` batch multipart request.
pub(crate) async fn post(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, GatewayHttpError> {
    authenticate(&headers, state.auth()).map_err(GatewayHttpError::unauthorized)?;
    state.metrics().batch_request();
    let key = parse_idempotency_key(&headers)?;
    let form = parse_multipart(multipart, state.limits().batch_upload_bytes).await?;
    let identity = form.identity()?;
    let recognizer = state
        .profiles()
        .batch(identity)
        .cloned()
        .ok_or_else(GatewayHttpError::unsupported_profile)?;

    let shared_audio = Arc::new(form.audio);
    let validation_audio = Arc::clone(&shared_audio);
    let validated = tokio::task::spawn_blocking(move || {
        validate_complete_ogg_opus(validation_audio.as_slice())
    })
    .await
    .map_err(|_| GatewayHttpError::unavailable("VALIDATION_UNAVAILABLE"))?
    .map_err(|_| GatewayHttpError::bad_request("INVALID_OGG_OPUS"))?;
    let audio = Arc::try_unwrap(shared_audio)
        .map_err(|_| GatewayHttpError::unavailable("VALIDATION_UNAVAILABLE"))?;
    let profile = profile(identity);
    let fingerprint = request_fingerprint(&profile, &form.keyterms, &audio)
        .map_err(|_| GatewayHttpError::bad_request("INVALID_BATCH_REQUEST"))?;
    let job_uuid = deterministic_job_id(&key);
    let job_id = BatchJobId::new(job_uuid.hyphenated().to_string());
    let coordinator = BatchCoordinator::new(recognizer.as_ref(), state.jobs(), state.spool());
    let outcome = coordinator
        .admit(BatchAdmissionRequest {
            id: job_id.clone(),
            profile,
            fingerprint,
            audio,
            authoritative_duration_millis: validated.duration_millis,
            keyterms: form.keyterms,
        })
        .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(failure @ BatchCoordinatorFailure::AdmissionConflict(_)) => {
            state.metrics().batch_conflict();
            return Err(map_coordinator_failure(&failure));
        }
        Err(failure) => {
            state.metrics().batch_failure();
            return Err(map_coordinator_failure(&failure));
        }
    };
    let snapshot = match outcome {
        BatchAdmissionOutcome::Accepted(snapshot) | BatchAdmissionOutcome::Replay(snapshot) => {
            snapshot
        }
    };
    if matches!(
        snapshot.job.state(),
        BatchJobState::Accepted | BatchJobState::Retryable { .. }
    ) {
        spawn_execution(state, identity, job_id);
    }
    response_for_snapshot(identity, job_uuid, &snapshot)
}

/// Loads one previously accepted batch job by its public UUID.
pub(crate) async fn get(
    State(state): State<GatewayState>,
    Path(job_id_path): Path<String>,
    headers: HeaderMap,
) -> Result<Response, GatewayHttpError> {
    authenticate(&headers, state.auth()).map_err(GatewayHttpError::unauthorized)?;
    state.metrics().batch_request();
    let job_uuid = Uuid::parse_str(&job_id_path).map_err(|_| GatewayHttpError::not_found())?;
    if job_uuid.hyphenated().to_string() != job_id_path {
        return Err(GatewayHttpError::not_found());
    }
    let job_id = BatchJobId::new(job_uuid.hyphenated().to_string());
    let loaded = state.jobs().load(&job_id).await;
    if loaded.is_err() {
        state.metrics().batch_failure();
    }
    let snapshot = loaded
        .map_err(|_| GatewayHttpError::unavailable("DATABASE_UNAVAILABLE"))?
        .ok_or_else(GatewayHttpError::not_found)?;
    let identity = identity_for_profile(snapshot.job.profile())
        .ok_or_else(|| GatewayHttpError::unavailable("INVALID_JOB_SNAPSHOT"))?;
    response_for_snapshot(identity, job_uuid, &snapshot)
}

fn spawn_execution(state: GatewayState, identity: BatchIdentity, id: BatchJobId) {
    tokio::spawn(async move {
        let Some(recognizer) = state.profiles().batch(identity).cloned() else {
            return;
        };
        let coordinator = BatchCoordinator::new(recognizer.as_ref(), state.jobs(), state.spool());
        if coordinator.execute(&id).await.is_err() {
            state.metrics().batch_failure();
            tracing::error!(job_id = %id.as_str(), "batch execution did not persist a safe outcome");
        }
    });
}

fn response_for_snapshot(
    identity: BatchIdentity,
    job_id: Uuid,
    snapshot: &BatchJobSnapshot,
) -> Result<Response, GatewayHttpError> {
    let outbound = match snapshot.job.state() {
        BatchJobState::Accepted | BatchJobState::Submitting { .. } => {
            OutboundBatchResponse::Pending {
                job_id,
                next_action: NextAction::Poll,
                retry_after_ms: DEFAULT_POLL_MILLIS,
            }
        }
        BatchJobState::Retryable { .. } => OutboundBatchResponse::Pending {
            job_id,
            next_action: NextAction::Retry,
            retry_after_ms: snapshot.retry_after_millis.unwrap_or(DEFAULT_RETRY_MILLIS),
        },
        BatchJobState::Failed { failure, .. } => OutboundBatchResponse::Failed {
            job_id,
            error_code: safe_failure_code(failure.code()),
        },
        BatchJobState::OutcomeUnknown { reason, .. } => OutboundBatchResponse::Failed {
            job_id,
            error_code: match reason {
                BatchUnknownOutcome::InterruptedSubmission => "SUBMISSION_INTERRUPTED".into(),
                BatchUnknownOutcome::Submission(_) => "PROVIDER_OUTCOME_UNKNOWN".into(),
            },
        },
        BatchJobState::Completed { .. } => {
            let result = snapshot
                .result
                .as_ref()
                .ok_or_else(|| GatewayHttpError::unavailable("INVALID_JOB_SNAPSHOT"))?;
            OutboundBatchResponse::Completed {
                job_id,
                result: OutboundTranscription {
                    text: result.text.clone(),
                    duration_millis: result.duration_millis,
                    segments: result
                        .segments
                        .iter()
                        .map(|segment| OutboundSegment {
                            start_millis: segment.start_millis,
                            end_millis: segment.end_millis,
                            text: segment.text.clone(),
                            confidence: segment.confidence.map(f64::from),
                        })
                        .collect(),
                    readable_segments: result
                        .readable_segments
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .map(|segment| OutboundReadableSegment {
                            start_millis: segment.start_millis,
                            end_millis: segment.end_millis,
                            text: segment.text.clone(),
                            source_segment_indices: segment.source_segment_indices.clone(),
                        })
                        .collect(),
                    provider_request_id: (identity == BatchIdentity::ElevenlabsScribeV2MultiV3)
                        .then(|| {
                            result
                                .provider_reference
                                .as_ref()
                                .map(|value| value.as_str().into())
                        })
                        .flatten(),
                },
            }
        }
    };
    let serialized = serialize_response(identity, &outbound)
        .map_err(|_| GatewayHttpError::unavailable("RESPONSE_PROJECTION_FAILED"))?;
    let status = match serialized.status {
        BatchResponseStatus::Accepted => StatusCode::ACCEPTED,
        BatchResponseStatus::Ok => StatusCode::OK,
    };
    Ok((
        status,
        [(CONTENT_TYPE, "application/json")],
        serialized.body,
    )
        .into_response())
}

struct BatchForm {
    contract_version: String,
    provider: String,
    model: String,
    language: String,
    keyterms: Vec<String>,
    audio: Vec<u8>,
}

impl BatchForm {
    fn identity(&self) -> Result<BatchIdentity, GatewayHttpError> {
        match (
            self.contract_version.as_str(),
            self.provider.as_str(),
            self.model.as_str(),
            self.language.as_str(),
        ) {
            ("2", "deepgram", "nova-3", "multi") => Ok(BatchIdentity::DeepgramNova3MultiV2),
            ("3", "elevenlabs", "scribe_v2", "multi") => {
                Ok(BatchIdentity::ElevenlabsScribeV2MultiV3)
            }
            _ => Err(GatewayHttpError::bad_request("UNSUPPORTED_PROFILE")),
        }
    }
}

async fn parse_multipart(
    mut multipart: Multipart,
    audio_limit: usize,
) -> Result<BatchForm, GatewayHttpError> {
    let mut text = Vec::with_capacity(5);
    for expected in [
        "contract_version",
        "provider",
        "model",
        "language",
        "keyterms",
    ] {
        let field = next_field(&mut multipart).await?;
        if field.name() != Some(expected) || field.file_name().is_some() {
            return Err(GatewayHttpError::bad_request("INVALID_MULTIPART_ORDER"));
        }
        text.push(read_text(field).await?);
    }
    let file = next_field(&mut multipart).await?;
    if file.name() != Some("file")
        || file.file_name() != Some("speaker-track.ogg")
        || file.content_type().map(ToString::to_string).as_deref() != Some("audio/ogg")
    {
        return Err(GatewayHttpError::bad_request("INVALID_AUDIO_PART"));
    }
    let audio = read_bounded(file, audio_limit).await?;
    if multipart
        .next_field()
        .await
        .map_err(|_| GatewayHttpError::bad_request("INVALID_MULTIPART"))?
        .is_some()
    {
        return Err(GatewayHttpError::bad_request("UNEXPECTED_MULTIPART_FIELD"));
    }
    let raw_keyterms: Vec<String> = serde_json::from_str(&text[4])
        .map_err(|_| GatewayHttpError::bad_request("INVALID_KEYTERMS"))?;
    let keyterms = canonicalize_keyterms(raw_keyterms)
        .map_err(|_| GatewayHttpError::bad_request("INVALID_KEYTERMS"))?;
    Ok(BatchForm {
        contract_version: text.remove(0),
        provider: text.remove(0),
        model: text.remove(0),
        language: text.remove(0),
        keyterms,
        audio,
    })
}

async fn next_field(multipart: &mut Multipart) -> Result<Field<'_>, GatewayHttpError> {
    multipart
        .next_field()
        .await
        .map_err(|_| GatewayHttpError::bad_request("INVALID_MULTIPART"))?
        .ok_or_else(|| GatewayHttpError::bad_request("MISSING_MULTIPART_FIELD"))
}

async fn read_text(field: Field<'_>) -> Result<String, GatewayHttpError> {
    let bytes = read_bounded(field, MAX_TEXT_FIELD_BYTES).await?;
    String::from_utf8(bytes).map_err(|_| GatewayHttpError::bad_request("INVALID_TEXT_FIELD"))
}

async fn read_bounded(mut field: Field<'_>, limit: usize) -> Result<Vec<u8>, GatewayHttpError> {
    let mut value = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| GatewayHttpError::bad_request("INVALID_MULTIPART"))?
    {
        if value.len().saturating_add(chunk.len()) > limit {
            return Err(GatewayHttpError::bad_request("MULTIPART_FIELD_TOO_LARGE"));
        }
        value.extend_from_slice(&chunk);
    }
    Ok(value)
}

fn parse_idempotency_key(headers: &HeaderMap) -> Result<IdempotencyKey, GatewayHttpError> {
    let mut values = headers.get_all(IDEMPOTENCY_HEADER).iter();
    let value = values
        .next()
        .ok_or_else(|| GatewayHttpError::bad_request("INVALID_IDEMPOTENCY_KEY"))?;
    if values.next().is_some() {
        return Err(GatewayHttpError::bad_request("INVALID_IDEMPOTENCY_KEY"));
    }
    let value = value
        .to_str()
        .map_err(|_| GatewayHttpError::bad_request("INVALID_IDEMPOTENCY_KEY"))?;
    IdempotencyKey::parse(value)
        .map_err(|_| GatewayHttpError::bad_request("INVALID_IDEMPOTENCY_KEY"))
}

fn profile(identity: BatchIdentity) -> BatchProfile {
    BatchProfile::new(
        u16::from(identity.contract_version()),
        identity.provider(),
        identity.model(),
        identity.language(),
    )
    .expect("frozen batch identity is valid")
}

fn identity_for_profile(profile: &BatchProfile) -> Option<BatchIdentity> {
    match (
        profile.contract_version(),
        profile.provider(),
        profile.model(),
        profile.language(),
    ) {
        (2, "deepgram", "nova-3", "multi") => Some(BatchIdentity::DeepgramNova3MultiV2),
        (3, "elevenlabs", "scribe_v2", "multi") => Some(BatchIdentity::ElevenlabsScribeV2MultiV3),
        _ => None,
    }
}

fn safe_failure_code(code: &str) -> String {
    if !code.is_empty()
        && code.len() <= 128
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        code.into()
    } else {
        "TRANSCRIPTION_FAILED".into()
    }
}

fn map_coordinator_failure(failure: &BatchCoordinatorFailure) -> GatewayHttpError {
    match failure {
        BatchCoordinatorFailure::AdmissionConflict(_) => GatewayHttpError::conflict(),
        BatchCoordinatorFailure::Spool(_) | BatchCoordinatorFailure::AdmissionCleanup { .. } => {
            GatewayHttpError::unavailable("SPOOL_UNAVAILABLE")
        }
        BatchCoordinatorFailure::Store(_)
        | BatchCoordinatorFailure::AdmissionStoreUncertain { .. }
        | BatchCoordinatorFailure::Transition(_)
        | BatchCoordinatorFailure::PostEgressStore(_)
        | BatchCoordinatorFailure::PostEgressConflict => {
            GatewayHttpError::unavailable("DATABASE_UNAVAILABLE")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalization_matches_client_shape() {
        assert_eq!(
            canonicalize_keyterms(vec![" beta ".into(), "alpha".into(), "alpha".into()]).unwrap(),
            vec!["alpha", "beta"]
        );
        assert!(canonicalize_keyterms(vec![" ".into()]).is_err());
        assert!(canonicalize_keyterms(vec!["x".repeat(201)]).is_err());
        assert_eq!(
            canonicalize_keyterms(vec!["\u{e000}".into(), "\u{1f600}".into()]).unwrap(),
            vec!["\u{1f600}", "\u{e000}"]
        );
    }

    #[test]
    fn only_frozen_profiles_are_selected() {
        assert_eq!(
            identity_for_profile(&profile(BatchIdentity::DeepgramNova3MultiV2)),
            Some(BatchIdentity::DeepgramNova3MultiV2)
        );
        let unsupported = BatchProfile::new(2, "deepgram", "nova-2", "multi").unwrap();
        assert_eq!(identity_for_profile(&unsupported), None);
    }

    #[test]
    fn unsafe_provider_codes_are_not_exposed() {
        assert_eq!(safe_failure_code("RATE_LIMIT"), "RATE_LIMIT");
        assert_eq!(
            safe_failure_code("provider said no"),
            "TRANSCRIPTION_FAILED"
        );
    }
}
