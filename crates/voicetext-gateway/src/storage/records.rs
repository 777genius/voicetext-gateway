//! Fail-closed mapping between `PostgreSQL` records and application-owned models.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use voicetext_speech::application::ports::{
    BatchAudioHandle, BatchJobId, BatchJobSnapshot, BatchReadableSegment, BatchRecognitionResult,
    BatchSegment, ProviderReference,
};
use voicetext_speech::domain::batch::{
    BatchFailure, BatchJob, BatchJobState, BatchProfile, BatchRequestFingerprint,
    BatchUnknownOutcome,
};

const MAX_KEYTERMS: usize = 100;
const MAX_KEYTERM_BYTES: usize = 256;
const MAX_KEYTERMS_BYTES: usize = 16_384;
const MAX_PROVIDER_REFERENCE_BYTES: usize = 256;
const MAX_PROFILE_FIELD_BYTES: usize = 256;
const MAX_SEGMENTS: usize = 10_000;
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_READABLE_REFERENCES: usize = 100_000;
const MAX_RETRY_AFTER_MILLIS: u64 = 3_600_000;
// This exact compact representation cap is owned by the PostgreSQL adapter. The same serialized
// bytes are stored as text and measured by the database constraint. It safely covers the
// application's 4 MiB content budget at JSON's worst six-byte escaping expansion, plus fewer than
// 20,001 bounded records and 100,000 four-digit source indices.
pub(crate) const MAX_SERIALIZED_RESULT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct JobRecord {
    pub job_id: Uuid,
    pub contract_version: i16,
    pub provider: String,
    pub model: String,
    pub language: String,
    pub fingerprint: Vec<u8>,
    pub audio_handle: String,
    pub authoritative_duration_millis: i64,
    pub keyterms: Value,
    pub state: String,
    pub attempt: Option<i64>,
    pub failure_code: Option<String>,
    pub unknown_reason: Option<String>,
    pub provider_reference: Option<String>,
    pub retry_after_millis: Option<i64>,
    pub result_json: Option<String>,
    pub revision: i64,
}

#[derive(Debug)]
pub(crate) struct WritableRecord {
    pub job_id: Uuid,
    pub contract_version: i16,
    pub provider: String,
    pub model: String,
    pub language: String,
    pub fingerprint: Vec<u8>,
    pub audio_handle: String,
    pub authoritative_duration_millis: i64,
    pub keyterms: Value,
    pub state: &'static str,
    pub attempt: Option<i64>,
    pub failure_code: Option<String>,
    pub unknown_reason: Option<&'static str>,
    pub provider_reference: Option<String>,
    pub retry_after_millis: Option<i64>,
    pub result_json: Option<String>,
    pub revision: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordError(pub(crate) &'static str);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ResultRecord {
    text: String,
    duration_millis: u64,
    provider_duration_millis: Option<u64>,
    segments: Vec<SegmentRecord>,
    readable_segments: Option<Vec<ReadableRecord>>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SegmentRecord {
    start_millis: u64,
    end_millis: u64,
    text: String,
    confidence: Option<f32>,
    speaker: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadableRecord {
    start_millis: u64,
    end_millis: u64,
    text: String,
    source_segment_indices: Vec<usize>,
}

impl TryFrom<JobRecord> for BatchJobSnapshot {
    type Error = RecordError;

    fn try_from(record: JobRecord) -> Result<Self, Self::Error> {
        let id = record.job_id.hyphenated().to_string();
        let contract_version = u16::try_from(record.contract_version)
            .map_err(|_| RecordError("INVALID_CONTRACT_VERSION"))?;
        let profile = BatchProfile::new(
            contract_version,
            record.provider,
            record.model,
            record.language,
        )
        .map_err(|_| RecordError("INVALID_PROFILE"))?;
        validate_profile(&profile)?;
        let fingerprint: [u8; 32] = record
            .fingerprint
            .try_into()
            .map_err(|_| RecordError("INVALID_FINGERPRINT"))?;
        validate_handle(&record.audio_handle, &id)?;
        let duration = u64::try_from(record.authoritative_duration_millis)
            .map_err(|_| RecordError("INVALID_DURATION"))?;
        let keyterms: Vec<String> =
            serde_json::from_value(record.keyterms).map_err(|_| RecordError("INVALID_KEYTERMS"))?;
        validate_keyterms(&keyterms)?;
        let provider_reference = reference(record.provider_reference)?;
        let state = restore_state(
            &record.state,
            record.attempt,
            record.failure_code,
            record.unknown_reason.as_deref(),
        )?;
        let result = restore_result(
            record.result_json,
            &profile,
            duration,
            provider_reference.clone(),
        )?;
        validate_result_presence(&state, result.is_some())?;
        let retry_after_millis = restore_retry_after(&state, record.retry_after_millis)?;
        let revision =
            u64::try_from(record.revision).map_err(|_| RecordError("INVALID_REVISION"))?;
        Ok(Self {
            id: BatchJobId::new(id),
            job: BatchJob::restore(
                profile,
                BatchRequestFingerprint::from_bytes(fingerprint),
                state,
            ),
            audio: BatchAudioHandle::new(record.audio_handle),
            authoritative_duration_millis: duration,
            keyterms,
            provider_reference,
            retry_after_millis,
            result,
            revision,
        })
    }
}

impl TryFrom<&BatchJobSnapshot> for WritableRecord {
    type Error = RecordError;

    fn try_from(snapshot: &BatchJobSnapshot) -> Result<Self, Self::Error> {
        let job_id = canonical_uuid(snapshot.id.as_str())?;
        validate_handle(snapshot.audio.as_str(), snapshot.id.as_str())?;
        validate_profile(snapshot.job.profile())?;
        validate_keyterms(&snapshot.keyterms)?;
        validate_reference(snapshot.provider_reference.as_ref())?;
        validate_retry_after(snapshot.job.state(), snapshot.retry_after_millis)?;
        let (state, attempt, failure_code, unknown_reason) = flatten_state(snapshot.job.state());
        let result_json = snapshot
            .result
            .as_ref()
            .map(|result| serialize_result(snapshot, result))
            .transpose()?;
        validate_result_presence(snapshot.job.state(), result_json.is_some())?;
        Ok(Self {
            job_id,
            contract_version: i16::try_from(snapshot.job.profile().contract_version())
                .map_err(|_| RecordError("INVALID_CONTRACT_VERSION"))?,
            provider: snapshot.job.profile().provider().into(),
            model: snapshot.job.profile().model().into(),
            language: snapshot.job.profile().language().into(),
            fingerprint: snapshot.job.fingerprint().as_bytes().to_vec(),
            audio_handle: snapshot.audio.as_str().into(),
            authoritative_duration_millis: i64::try_from(snapshot.authoritative_duration_millis)
                .map_err(|_| RecordError("INVALID_DURATION"))?,
            keyterms: serde_json::to_value(&snapshot.keyterms)
                .map_err(|_| RecordError("INVALID_KEYTERMS"))?,
            state,
            attempt,
            failure_code,
            unknown_reason,
            provider_reference: snapshot
                .provider_reference
                .as_ref()
                .map(|value| value.as_str().into()),
            retry_after_millis: snapshot
                .retry_after_millis
                .map(i64::try_from)
                .transpose()
                .map_err(|_| RecordError("INVALID_RETRY_AFTER"))?,
            result_json,
            revision: i64::try_from(snapshot.revision)
                .map_err(|_| RecordError("INVALID_REVISION"))?,
        })
    }
}

fn canonical_uuid(value: &str) -> Result<Uuid, RecordError> {
    let uuid = Uuid::parse_str(value).map_err(|_| RecordError("INVALID_JOB_ID"))?;
    if uuid.hyphenated().to_string() != value {
        return Err(RecordError("INVALID_JOB_ID"));
    }
    Ok(uuid)
}

fn validate_handle(handle: &str, job_id: &str) -> Result<(), RecordError> {
    let expected_prefix = format!("{job_id}-");
    let digest = handle
        .strip_prefix(&expected_prefix)
        .and_then(|value| value.strip_suffix(".ogg"))
        .ok_or(RecordError("INVALID_AUDIO_HANDLE"))?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RecordError("INVALID_AUDIO_HANDLE"));
    }
    Ok(())
}

fn validate_keyterms(keyterms: &[String]) -> Result<(), RecordError> {
    if keyterms.len() > MAX_KEYTERMS {
        return Err(RecordError("INVALID_KEYTERMS"));
    }
    let mut total = 0_usize;
    for keyterm in keyterms {
        if keyterm.is_empty()
            || keyterm.len() > MAX_KEYTERM_BYTES
            || keyterm.trim() != keyterm
            || keyterm.chars().any(char::is_control)
        {
            return Err(RecordError("INVALID_KEYTERMS"));
        }
        total = total
            .checked_add(keyterm.len())
            .ok_or(RecordError("INVALID_KEYTERMS"))?;
    }
    if total > MAX_KEYTERMS_BYTES {
        return Err(RecordError("INVALID_KEYTERMS"));
    }
    Ok(())
}

fn validate_profile(profile: &BatchProfile) -> Result<(), RecordError> {
    if [profile.provider(), profile.model(), profile.language()]
        .into_iter()
        .any(|value| value.len() > MAX_PROFILE_FIELD_BYTES)
    {
        return Err(RecordError("INVALID_PROFILE"));
    }
    Ok(())
}

fn reference(value: Option<String>) -> Result<Option<ProviderReference>, RecordError> {
    let reference = value.map(ProviderReference::new);
    validate_reference(reference.as_ref())?;
    Ok(reference)
}

fn validate_reference(value: Option<&ProviderReference>) -> Result<(), RecordError> {
    if let Some(value) = value {
        let value = value.as_str();
        if value.is_empty()
            || value.len() > MAX_PROVIDER_REFERENCE_BYTES
            || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        {
            return Err(RecordError("INVALID_PROVIDER_REFERENCE"));
        }
    }
    Ok(())
}

fn flatten_state(
    state: &BatchJobState,
) -> (
    &'static str,
    Option<i64>,
    Option<String>,
    Option<&'static str>,
) {
    match state {
        BatchJobState::Accepted => ("accepted", None, None, None),
        BatchJobState::Submitting { attempt } => {
            ("submitting", Some(i64::from(attempt.get())), None, None)
        }
        BatchJobState::Retryable { attempt, failure } => (
            "retryable",
            Some(i64::from(attempt.get())),
            Some(failure.code().into()),
            None,
        ),
        BatchJobState::Completed { attempt } => {
            ("completed", Some(i64::from(attempt.get())), None, None)
        }
        BatchJobState::Failed { attempt, failure } => (
            "failed",
            Some(i64::from(attempt.get())),
            Some(failure.code().into()),
            None,
        ),
        BatchJobState::OutcomeUnknown { attempt, reason } => match reason {
            BatchUnknownOutcome::Submission(failure) => (
                "outcome_unknown",
                Some(i64::from(attempt.get())),
                Some(failure.code().into()),
                Some("submission"),
            ),
            BatchUnknownOutcome::InterruptedSubmission => (
                "outcome_unknown",
                Some(i64::from(attempt.get())),
                None,
                Some("interrupted_submission"),
            ),
        },
    }
}

fn restore_state(
    state: &str,
    attempt: Option<i64>,
    failure_code: Option<String>,
    unknown_reason: Option<&str>,
) -> Result<BatchJobState, RecordError> {
    let attempt = attempt
        .map(|value| {
            u32::try_from(value)
                .ok()
                .and_then(NonZeroU32::new)
                .ok_or(RecordError("INVALID_ATTEMPT"))
        })
        .transpose()?;
    let failure = failure_code
        .map(|value| BatchFailure::new(value).map_err(|_| RecordError("INVALID_FAILURE_CODE")))
        .transpose()?;
    match (state, attempt, failure, unknown_reason) {
        ("accepted", None, None, None) => Ok(BatchJobState::Accepted),
        ("submitting", Some(attempt), None, None) => Ok(BatchJobState::Submitting { attempt }),
        ("retryable", Some(attempt), Some(failure), None) => {
            Ok(BatchJobState::Retryable { attempt, failure })
        }
        ("completed", Some(attempt), None, None) => Ok(BatchJobState::Completed { attempt }),
        ("failed", Some(attempt), Some(failure), None) => {
            Ok(BatchJobState::Failed { attempt, failure })
        }
        ("outcome_unknown", Some(attempt), Some(failure), Some("submission")) => {
            Ok(BatchJobState::OutcomeUnknown {
                attempt,
                reason: BatchUnknownOutcome::Submission(failure),
            })
        }
        ("outcome_unknown", Some(attempt), None, Some("interrupted_submission")) => {
            Ok(BatchJobState::OutcomeUnknown {
                attempt,
                reason: BatchUnknownOutcome::InterruptedSubmission,
            })
        }
        _ => Err(RecordError("INVALID_STATE_SNAPSHOT")),
    }
}

fn serialize_result(
    snapshot: &BatchJobSnapshot,
    result: &BatchRecognitionResult,
) -> Result<String, RecordError> {
    if result.profile != *snapshot.job.profile()
        || result.duration_millis != snapshot.authoritative_duration_millis
        || result.provider_reference != snapshot.provider_reference
    {
        return Err(RecordError("INVALID_RESULT_IDENTITY"));
    }
    validate_result(result)?;
    let record = ResultRecord {
        text: result.text.clone(),
        duration_millis: result.duration_millis,
        provider_duration_millis: result.provider_duration_millis,
        segments: result
            .segments
            .iter()
            .map(|segment| SegmentRecord {
                start_millis: segment.start_millis,
                end_millis: segment.end_millis,
                text: segment.text.clone(),
                confidence: segment.confidence,
                speaker: segment.speaker.clone(),
            })
            .collect(),
        readable_segments: result.readable_segments.as_ref().map(|segments| {
            segments
                .iter()
                .map(|segment| ReadableRecord {
                    start_millis: segment.start_millis,
                    end_millis: segment.end_millis,
                    text: segment.text.clone(),
                    source_segment_indices: segment.source_segment_indices.clone(),
                })
                .collect()
        }),
    };
    let value = serde_json::to_string(&record).map_err(|_| RecordError("INVALID_RESULT_JSON"))?;
    bounded_json(&value)?;
    Ok(value)
}

fn restore_result(
    value: Option<String>,
    profile: &BatchProfile,
    duration: u64,
    provider_reference: Option<ProviderReference>,
) -> Result<Option<BatchRecognitionResult>, RecordError> {
    let Some(value) = value else { return Ok(None) };
    bounded_json(&value)?;
    let record: ResultRecord =
        serde_json::from_str(&value).map_err(|_| RecordError("INVALID_RESULT_JSON"))?;
    let result = BatchRecognitionResult {
        profile: profile.clone(),
        text: record.text,
        duration_millis: record.duration_millis,
        provider_duration_millis: record.provider_duration_millis,
        segments: record
            .segments
            .into_iter()
            .map(|segment| BatchSegment {
                start_millis: segment.start_millis,
                end_millis: segment.end_millis,
                text: segment.text,
                confidence: segment.confidence,
                speaker: segment.speaker,
            })
            .collect(),
        readable_segments: record.readable_segments.map(|segments| {
            segments
                .into_iter()
                .map(|segment| BatchReadableSegment {
                    start_millis: segment.start_millis,
                    end_millis: segment.end_millis,
                    text: segment.text,
                    source_segment_indices: segment.source_segment_indices,
                })
                .collect()
        }),
        provider_reference,
    };
    if result.duration_millis != duration {
        return Err(RecordError("INVALID_RESULT_IDENTITY"));
    }
    validate_result(&result)?;
    Ok(Some(result))
}

fn bounded_json(value: &str) -> Result<(), RecordError> {
    if value.len() > MAX_SERIALIZED_RESULT_BYTES {
        return Err(RecordError("RESULT_TOO_LARGE"));
    }
    Ok(())
}

fn validate_result(result: &BatchRecognitionResult) -> Result<(), RecordError> {
    if result.text.len() > MAX_TEXT_BYTES || result.segments.len() > MAX_SEGMENTS {
        return Err(RecordError("RESULT_TOO_LARGE"));
    }
    let mut previous_end = 0;
    for segment in &result.segments {
        if segment.start_millis < previous_end
            || segment.end_millis <= segment.start_millis
            || segment.end_millis > result.duration_millis
            || segment.text.is_empty()
            || segment.text.len() > MAX_TEXT_BYTES
            || segment
                .confidence
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            || segment.speaker.as_ref().is_some_and(|value| {
                value.is_empty()
                    || value.len() > MAX_PROVIDER_REFERENCE_BYTES
                    || value.chars().any(char::is_control)
            })
        {
            return Err(RecordError("INVALID_RESULT"));
        }
        previous_end = segment.end_millis;
    }
    let mut references = 0_usize;
    if let Some(segments) = &result.readable_segments {
        if segments.len() > MAX_SEGMENTS {
            return Err(RecordError("RESULT_TOO_LARGE"));
        }
        previous_end = 0;
        for segment in segments {
            references = references
                .checked_add(segment.source_segment_indices.len())
                .ok_or(RecordError("RESULT_TOO_LARGE"))?;
            if segment.start_millis < previous_end
                || segment.end_millis <= segment.start_millis
                || segment.end_millis > result.duration_millis
                || segment.text.is_empty()
                || segment.text.len() > MAX_TEXT_BYTES
                || segment.source_segment_indices.is_empty()
                || !segment
                    .source_segment_indices
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
                || segment
                    .source_segment_indices
                    .last()
                    .is_none_or(|index| *index >= result.segments.len())
            {
                return Err(RecordError("INVALID_RESULT"));
            }
            previous_end = segment.end_millis;
        }
    }
    if references > MAX_READABLE_REFERENCES {
        return Err(RecordError("RESULT_TOO_LARGE"));
    }
    Ok(())
}

fn validate_result_presence(state: &BatchJobState, has_result: bool) -> Result<(), RecordError> {
    if matches!(state, BatchJobState::Completed { .. }) != has_result {
        return Err(RecordError("INVALID_RESULT_STATE"));
    }
    Ok(())
}

fn validate_retry_after(
    state: &BatchJobState,
    retry_after_millis: Option<u64>,
) -> Result<(), RecordError> {
    if retry_after_millis.is_some_and(|value| value > MAX_RETRY_AFTER_MILLIS)
        || (!matches!(state, BatchJobState::Retryable { .. }) && retry_after_millis.is_some())
    {
        return Err(RecordError("INVALID_RETRY_AFTER"));
    }
    Ok(())
}

fn restore_retry_after(
    state: &BatchJobState,
    value: Option<i64>,
) -> Result<Option<u64>, RecordError> {
    let value = value
        .map(u64::try_from)
        .transpose()
        .map_err(|_| RecordError("INVALID_RETRY_AFTER"))?;
    validate_retry_after(state, value)?;
    Ok(value)
}
