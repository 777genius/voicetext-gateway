//! Exact batch v2 and v3 response contracts consumed by Discord Meeting Assistant.

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use super::ContractViolation;

const MAX_ERROR_CODE_CHARS: usize = 128;
const MAX_PROVIDER_REQUEST_ID_CHARS: usize = 256;
const MAX_READABLE_REFERENCES: usize = 100_000;
const MAX_SEGMENTS: usize = 10_000;
const MAX_TRANSCRIPT_CHARS: usize = 1_000_000;
const MAX_RETRY_AFTER_MS: u64 = 3_600_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// The only supported batch contract identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchIdentity {
    DeepgramNova3MultiV2,
    ElevenlabsScribeV2MultiV3,
}

impl BatchIdentity {
    pub const fn contract_version(self) -> u8 {
        match self {
            Self::DeepgramNova3MultiV2 => 2,
            Self::ElevenlabsScribeV2MultiV3 => 3,
        }
    }

    pub const fn provider(self) -> &'static str {
        match self {
            Self::DeepgramNova3MultiV2 => "deepgram",
            Self::ElevenlabsScribeV2MultiV3 => "elevenlabs",
        }
    }

    pub const fn model(self) -> &'static str {
        match self {
            Self::DeepgramNova3MultiV2 => "nova-3",
            Self::ElevenlabsScribeV2MultiV3 => "scribe_v2",
        }
    }

    pub const fn language(self) -> &'static str {
        "multi"
    }
}

/// HTTP status accompanying a decoded batch payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchResponseStatus {
    Accepted,
    Ok,
}

/// Validated projection shared by the current TypeScript consumer profiles.
#[derive(Clone, Debug, PartialEq)]
pub enum BatchTaskResult {
    Pending {
        job_id: Uuid,
        next_action: NextAction,
        retry_after_ms: u64,
    },
    Failed {
        job_id: Uuid,
        error_code: String,
    },
    Completed {
        job_id: Uuid,
        result: BatchTranscription,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    Poll,
    Retry,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BatchTranscription {
    pub duration_seconds: f64,
    pub utterances: Vec<Utterance>,
    pub readable_segments: Vec<ReadableSegment>,
    pub provider_request_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Utterance {
    #[serde(rename = "start", alias = "start_ms")]
    pub start: f64,
    #[serde(rename = "end", alias = "end_ms")]
    pub end: f64,
    #[serde(rename = "transcript", alias = "text")]
    pub transcript: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadableSegment {
    pub start: f64,
    pub end: f64,
    pub transcript: String,
    pub source_utterance_indices: Vec<usize>,
}

/// Decodes one bounded batch response and rejects profile or job identity drift.
///
/// # Errors
///
/// Returns [`ContractViolation`] for malformed JSON, unsupported states, identity drift, invalid
/// timelines, or any field that exceeds the compatibility bounds.
pub fn parse_response(
    json: &str,
    status: BatchResponseStatus,
    expected: BatchIdentity,
) -> Result<BatchTaskResult, ContractViolation> {
    if json.len() > 2 * 1_024 * 1_024 {
        return Err(ContractViolation("batch response exceeds byte limit"));
    }
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|_| ContractViolation("invalid batch JSON"))?;
    match expected {
        BatchIdentity::DeepgramNova3MultiV2 => parse_v2(value, status, expected),
        BatchIdentity::ElevenlabsScribeV2MultiV3 => parse_v3(value, status, expected),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingV2 {
    success: bool,
    status: String,
    #[serde(deserialize_with = "deserialize_job_id")]
    job_id: Uuid,
    next_action: NextAction,
    retry_after_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalV2 {
    success: bool,
    status: String,
    #[serde(deserialize_with = "deserialize_job_id")]
    job_id: Uuid,
    #[serde(default)]
    retryable: Option<bool>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    result: Option<ResultV2>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultV2 {
    provider: String,
    model: String,
    language: String,
    text: String,
    duration_seconds: f64,
    utterances: Vec<Utterance>,
    #[serde(default)]
    readable_segments: Vec<ReadableSegment>,
}

fn parse_v2(
    value: serde_json::Value,
    status: BatchResponseStatus,
    expected: BatchIdentity,
) -> Result<BatchTaskResult, ContractViolation> {
    if status == BatchResponseStatus::Accepted {
        let response: PendingV2 = serde_json::from_value(value)
            .map_err(|_| ContractViolation("invalid batch v2 pending response"))?;
        require(
            response.success && response.status == "running",
            "invalid batch v2 state",
        )?;
        require(
            response.retry_after_ms <= MAX_RETRY_AFTER_MS,
            "retry delay exceeds limit",
        )?;
        return Ok(BatchTaskResult::Pending {
            job_id: response.job_id,
            next_action: response.next_action,
            retry_after_ms: response.retry_after_ms,
        });
    }
    let response: FinalV2 = serde_json::from_value(value)
        .map_err(|_| ContractViolation("invalid batch v2 final response"))?;
    if !response.success && response.status == "failed" && response.retryable == Some(false) {
        let error_code = response
            .error_code
            .ok_or(ContractViolation("missing error code"))?;
        require(response.result.is_none(), "failed response contains result")?;
        validate_error_code(&error_code)?;
        return Ok(BatchTaskResult::Failed {
            job_id: response.job_id,
            error_code,
        });
    }
    require(
        response.success
            && response.status == "completed"
            && response.retryable.is_none()
            && response.error_code.is_none(),
        "invalid batch v2 completed state",
    )?;
    let result = response
        .result
        .ok_or(ContractViolation("missing batch result"))?;
    require_identity(&result.provider, &result.model, &result.language, expected)?;
    validate_text(&result.text, true)?;
    let transcription = validate_v2_result(result)?;
    Ok(BatchTaskResult::Completed {
        job_id: response.job_id,
        result: transcription,
    })
}

fn validate_v2_result(result: ResultV2) -> Result<BatchTranscription, ContractViolation> {
    require(
        result.duration_seconds.is_finite() && result.duration_seconds >= 0.0,
        "invalid duration",
    )?;
    require(
        result.utterances.len() <= MAX_SEGMENTS,
        "too many utterances",
    )?;
    for utterance in &result.utterances {
        validate_utterance(utterance)?;
    }
    let readable_segments = validate_readable_segments(
        result.readable_segments,
        result.duration_seconds,
        result.utterances.len(),
    )?;
    Ok(BatchTranscription {
        duration_seconds: result.duration_seconds,
        utterances: result.utterances,
        readable_segments,
        provider_request_id: None,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvelopeV3 {
    contract_version: u8,
    provider: String,
    model: String,
    language: String,
    success: bool,
    status: String,
    #[serde(deserialize_with = "deserialize_job_id")]
    job_id: Uuid,
    #[serde(default)]
    next_action: Option<NextAction>,
    #[serde(default)]
    retry_after_ms: Option<u64>,
    #[serde(default)]
    retryable: Option<bool>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    result: Option<ResultV3>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultV3 {
    #[serde(deserialize_with = "deserialize_job_id")]
    result_id: Uuid,
    provider: String,
    model: String,
    language: String,
    text: String,
    duration_ms: u64,
    segments: Vec<SegmentV3>,
    #[serde(default)]
    provider_request: Option<ProviderRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentV3 {
    index: usize,
    start_ms: u64,
    end_ms: u64,
    text: String,
    #[serde(default)]
    confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderRequest {
    id: String,
}

fn parse_v3(
    value: serde_json::Value,
    http_status: BatchResponseStatus,
    expected: BatchIdentity,
) -> Result<BatchTaskResult, ContractViolation> {
    let response: EnvelopeV3 = serde_json::from_value(value)
        .map_err(|_| ContractViolation("invalid batch v3 response"))?;
    require(
        response.contract_version == expected.contract_version(),
        "contract version mismatch",
    )?;
    require_identity(
        &response.provider,
        &response.model,
        &response.language,
        expected,
    )?;
    if http_status == BatchResponseStatus::Accepted {
        require(
            response.success && response.status == "running",
            "invalid batch v3 pending state",
        )?;
        require(
            response.retryable.is_none()
                && response.error_code.is_none()
                && response.result.is_none(),
            "pending response contains terminal fields",
        )?;
        let retry_after_ms = response
            .retry_after_ms
            .ok_or(ContractViolation("missing retry delay"))?;
        require(
            retry_after_ms <= MAX_RETRY_AFTER_MS,
            "retry delay exceeds limit",
        )?;
        return Ok(BatchTaskResult::Pending {
            job_id: response.job_id,
            next_action: response
                .next_action
                .ok_or(ContractViolation("missing next action"))?,
            retry_after_ms,
        });
    }
    require(
        response.next_action.is_none() && response.retry_after_ms.is_none(),
        "terminal response contains polling fields",
    )?;
    if !response.success && response.status == "failed" && response.retryable == Some(false) {
        require(response.result.is_none(), "failed response contains result")?;
        let error_code = response
            .error_code
            .ok_or(ContractViolation("missing error code"))?;
        validate_error_code(&error_code)?;
        return Ok(BatchTaskResult::Failed {
            job_id: response.job_id,
            error_code,
        });
    }
    require(
        response.success
            && response.status == "completed"
            && response.retryable.is_none()
            && response.error_code.is_none(),
        "invalid batch v3 completed state",
    )?;
    let result = response
        .result
        .ok_or(ContractViolation("missing batch result"))?;
    require(
        result.result_id == response.job_id,
        "batch result identity mismatch",
    )?;
    require_identity(&result.provider, &result.model, &result.language, expected)?;
    let transcription = validate_v3_result(result)?;
    Ok(BatchTaskResult::Completed {
        job_id: response.job_id,
        result: transcription,
    })
}

fn validate_v3_result(result: ResultV3) -> Result<BatchTranscription, ContractViolation> {
    validate_text(&result.text, true)?;
    require(
        result.text.is_empty() || !result.text.trim().is_empty(),
        "batch text cannot contain only whitespace",
    )?;
    require(
        result.duration_ms <= MAX_SAFE_INTEGER,
        "duration exceeds safe integer",
    )?;
    require(result.segments.len() <= MAX_SEGMENTS, "too many segments")?;
    require(
        result.text.is_empty() == result.segments.is_empty(),
        "text and segments disagree",
    )?;
    let mut previous_end = 0;
    let mut utterances = Vec::with_capacity(result.segments.len());
    for (expected_index, segment) in result.segments.into_iter().enumerate() {
        require(
            segment.index == expected_index,
            "non-contiguous segment index",
        )?;
        require(
            segment.start_ms >= previous_end
                && segment.end_ms > segment.start_ms
                && segment.end_ms <= result.duration_ms,
            "invalid segment timeline",
        )?;
        validate_text(&segment.text, false)?;
        validate_confidence(segment.confidence)?;
        previous_end = segment.end_ms;
        utterances.push(Utterance {
            start: milliseconds_to_seconds(segment.start_ms),
            end: milliseconds_to_seconds(segment.end_ms),
            transcript: segment.text,
            confidence: segment.confidence,
        });
    }
    let provider_request_id = result.provider_request.map(|request| request.id);
    if let Some(id) = &provider_request_id {
        require(
            !id.is_empty()
                && char_len(id) <= MAX_PROVIDER_REQUEST_ID_CHARS
                && id.bytes().all(|byte| (0x20..=0x7e).contains(&byte)),
            "invalid provider request id",
        )?;
    }
    Ok(BatchTranscription {
        duration_seconds: milliseconds_to_seconds(result.duration_ms),
        utterances,
        readable_segments: Vec::new(),
        provider_request_id,
    })
}

fn validate_readable_segments(
    segments: Vec<ReadableSegment>,
    duration: f64,
    utterance_count: usize,
) -> Result<Vec<ReadableSegment>, ContractViolation> {
    require(segments.len() <= MAX_SEGMENTS, "too many readable segments")?;
    let mut references = 0_usize;
    let mut characters = 0_usize;
    for segment in &segments {
        validate_text(&segment.transcript, false)?;
        require(
            segment.start.is_finite()
                && segment.start >= 0.0
                && segment.end.is_finite()
                && segment.end > segment.start
                && segment.end <= duration,
            "invalid readable segment timeline",
        )?;
        require(
            !segment.source_utterance_indices.is_empty(),
            "readable segment has no sources",
        )?;
        let mut previous = None;
        for &index in &segment.source_utterance_indices {
            require(
                index < utterance_count && previous.is_none_or(|value| index > value),
                "invalid readable segment source",
            )?;
            previous = Some(index);
        }
        references = references
            .checked_add(segment.source_utterance_indices.len())
            .ok_or(ContractViolation("too many readable references"))?;
        characters = characters
            .checked_add(char_len(&segment.transcript))
            .ok_or(ContractViolation("too much readable text"))?;
    }
    require(
        references <= MAX_READABLE_REFERENCES && characters <= MAX_TRANSCRIPT_CHARS,
        "readable projection exceeds bounds",
    )?;
    Ok(segments)
}

fn validate_utterance(utterance: &Utterance) -> Result<(), ContractViolation> {
    require(
        utterance.start.is_finite()
            && utterance.start >= 0.0
            && utterance.end.is_finite()
            && utterance.end >= utterance.start,
        "invalid utterance timeline",
    )?;
    validate_text(&utterance.transcript, true)?;
    validate_confidence(utterance.confidence)
}

fn deserialize_job_id<'de, D>(deserializer: D) -> Result<Uuid, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    let bytes = value.as_bytes();
    let canonical = bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit());
    if !canonical {
        return Err(serde::de::Error::custom("job id must be a canonical UUID"));
    }
    Uuid::parse_str(&value).map_err(serde::de::Error::custom)
}

#[allow(clippy::cast_precision_loss)]
fn milliseconds_to_seconds(milliseconds: u64) -> f64 {
    milliseconds as f64 / 1_000.0
}

fn validate_confidence(value: Option<f64>) -> Result<(), ContractViolation> {
    require(
        value.is_none_or(|confidence| confidence.is_finite() && (0.0..=1.0).contains(&confidence)),
        "invalid confidence",
    )
}

fn require_identity(
    provider: &str,
    model: &str,
    language: &str,
    expected: BatchIdentity,
) -> Result<(), ContractViolation> {
    require(
        provider == expected.provider()
            && model == expected.model()
            && language == expected.language(),
        "batch identity mismatch",
    )
}

fn validate_error_code(value: &str) -> Result<(), ContractViolation> {
    require(
        !value.is_empty()
            && char_len(value) <= MAX_ERROR_CODE_CHARS
            && value
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'),
        "invalid error code",
    )
}

fn validate_text(value: &str, allow_empty: bool) -> Result<(), ContractViolation> {
    require(
        char_len(value) <= MAX_TRANSCRIPT_CHARS && (allow_empty || !value.trim().is_empty()),
        "invalid transcript text",
    )
}

fn char_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn require(condition: bool, message: &'static str) -> Result<(), ContractViolation> {
    condition.then_some(()).ok_or(ContractViolation(message))
}
