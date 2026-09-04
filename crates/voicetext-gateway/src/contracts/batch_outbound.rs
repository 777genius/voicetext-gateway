//! Validated server projection for batch v2 and v3 response JSON.

use serde::Serialize;
use uuid::Uuid;

use super::ContractViolation;
use super::batch::{BatchIdentity, BatchResponseStatus, NextAction};

const MAX_ERROR_CODE_CHARS: usize = 128;
const MAX_PROVIDER_REQUEST_ID_CHARS: usize = 256;
const MAX_READABLE_REFERENCES: usize = 100_000;
const MAX_SEGMENTS: usize = 10_000;
const MAX_TRANSCRIPT_CHARS: usize = 1_000_000;
const MAX_RETRY_AFTER_MS: u64 = 3_600_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// One provider-neutral segment ready for contract-specific projection.
#[derive(Clone, Debug, PartialEq)]
pub struct OutboundSegment {
    pub start_millis: u64,
    pub end_millis: u64,
    pub text: String,
    pub confidence: Option<f64>,
}

/// Optional readable v2 projection linked to source segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundReadableSegment {
    pub start_millis: u64,
    pub end_millis: u64,
    pub text: String,
    pub source_segment_indices: Vec<usize>,
}

/// Complete validated data needed to emit a batch result in either supported contract.
#[derive(Clone, Debug, PartialEq)]
pub struct OutboundTranscription {
    pub text: String,
    pub duration_millis: u64,
    pub segments: Vec<OutboundSegment>,
    pub readable_segments: Vec<OutboundReadableSegment>,
    pub provider_request_id: Option<String>,
}

/// Server-owned batch state; retryability is fixed by the published contract.
#[derive(Clone, Debug, PartialEq)]
pub enum OutboundBatchResponse {
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
        result: OutboundTranscription,
    },
}

/// JSON body paired with the only HTTP status valid for its lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerializedBatchResponse {
    pub status: BatchResponseStatus,
    pub body: String,
}

/// Validates and serializes one exact v2 or v3 server response.
///
/// # Errors
///
/// Returns [`ContractViolation`] for invalid timelines, non-finite values, unrepresentable
/// profile-specific fields, resource-bound violations, or serialization failure.
pub fn serialize_response(
    identity: BatchIdentity,
    response: &OutboundBatchResponse,
) -> Result<SerializedBatchResponse, ContractViolation> {
    let (status, body) = match response {
        OutboundBatchResponse::Pending {
            job_id,
            next_action,
            retry_after_ms,
        } => {
            require(
                *retry_after_ms <= MAX_RETRY_AFTER_MS,
                "retry delay exceeds limit",
            )?;
            let status = BatchResponseStatus::Accepted;
            let body = match identity {
                BatchIdentity::DeepgramNova3MultiV2 => serde_json::to_string(&PendingV2 {
                    success: true,
                    status: "running",
                    job_id,
                    next_action: *next_action,
                    retry_after_ms: *retry_after_ms,
                }),
                BatchIdentity::ElevenlabsScribeV2MultiV3 => serde_json::to_string(&EnvelopeV3 {
                    contract_version: 3,
                    provider: identity.provider(),
                    model: identity.model(),
                    language: identity.language(),
                    state: StateV3::Pending {
                        success: true,
                        status: "running",
                        job_id,
                        next_action: *next_action,
                        retry_after_ms: *retry_after_ms,
                    },
                }),
            };
            (status, body)
        }
        OutboundBatchResponse::Failed { job_id, error_code } => {
            validate_error_code(error_code)?;
            let status = BatchResponseStatus::Ok;
            let body = match identity {
                BatchIdentity::DeepgramNova3MultiV2 => serde_json::to_string(&FailedV2 {
                    success: false,
                    status: "failed",
                    job_id,
                    retryable: false,
                    error_code,
                }),
                BatchIdentity::ElevenlabsScribeV2MultiV3 => serde_json::to_string(&EnvelopeV3 {
                    contract_version: 3,
                    provider: identity.provider(),
                    model: identity.model(),
                    language: identity.language(),
                    state: StateV3::Failed {
                        success: false,
                        status: "failed",
                        job_id,
                        retryable: false,
                        error_code,
                    },
                }),
            };
            (status, body)
        }
        OutboundBatchResponse::Completed { job_id, result } => {
            validate_result(identity, result)?;
            let status = BatchResponseStatus::Ok;
            let body = match identity {
                BatchIdentity::DeepgramNova3MultiV2 => serialize_completed_v2(*job_id, result),
                BatchIdentity::ElevenlabsScribeV2MultiV3 => {
                    serialize_completed_v3(identity, *job_id, result)
                }
            };
            (status, body)
        }
    };
    let body = body.map_err(|_| ContractViolation("cannot serialize batch response"))?;
    require(
        body.len() <= 2 * 1_024 * 1_024,
        "batch response exceeds byte limit",
    )?;
    Ok(SerializedBatchResponse { status, body })
}

#[derive(Serialize)]
struct PendingV2<'a> {
    success: bool,
    status: &'static str,
    job_id: &'a Uuid,
    next_action: NextAction,
    retry_after_ms: u64,
}

#[derive(Serialize)]
struct FailedV2<'a> {
    success: bool,
    status: &'static str,
    job_id: &'a Uuid,
    retryable: bool,
    error_code: &'a str,
}

#[derive(Serialize)]
struct CompletedV2<'a> {
    success: bool,
    status: &'static str,
    job_id: Uuid,
    result: ResultV2<'a>,
}

#[derive(Serialize)]
struct ResultV2<'a> {
    provider: &'static str,
    model: &'static str,
    language: &'static str,
    text: &'a str,
    duration_seconds: f64,
    utterances: Vec<UtteranceV2<'a>>,
    readable_segments: Vec<ReadableV2<'a>>,
}

#[derive(Serialize)]
struct UtteranceV2<'a> {
    start: f64,
    end: f64,
    transcript: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f64>,
}

#[derive(Serialize)]
struct ReadableV2<'a> {
    start: f64,
    end: f64,
    transcript: &'a str,
    source_utterance_indices: &'a [usize],
}

fn serialize_completed_v2(
    job_id: Uuid,
    result: &OutboundTranscription,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&CompletedV2 {
        success: true,
        status: "completed",
        job_id,
        result: ResultV2 {
            provider: BatchIdentity::DeepgramNova3MultiV2.provider(),
            model: BatchIdentity::DeepgramNova3MultiV2.model(),
            language: BatchIdentity::DeepgramNova3MultiV2.language(),
            text: &result.text,
            duration_seconds: seconds(result.duration_millis),
            utterances: result
                .segments
                .iter()
                .map(|segment| UtteranceV2 {
                    start: seconds(segment.start_millis),
                    end: seconds(segment.end_millis),
                    transcript: &segment.text,
                    confidence: segment.confidence,
                })
                .collect(),
            readable_segments: result
                .readable_segments
                .iter()
                .map(|segment| ReadableV2 {
                    start: seconds(segment.start_millis),
                    end: seconds(segment.end_millis),
                    transcript: &segment.text,
                    source_utterance_indices: &segment.source_segment_indices,
                })
                .collect(),
        },
    })
}

#[derive(Serialize)]
struct EnvelopeV3<'a> {
    contract_version: u8,
    provider: &'static str,
    model: &'static str,
    language: &'static str,
    #[serde(flatten)]
    state: StateV3<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum StateV3<'a> {
    Pending {
        success: bool,
        status: &'static str,
        job_id: &'a Uuid,
        next_action: NextAction,
        retry_after_ms: u64,
    },
    Failed {
        success: bool,
        status: &'static str,
        job_id: &'a Uuid,
        retryable: bool,
        error_code: &'a str,
    },
    Completed {
        success: bool,
        status: &'static str,
        job_id: Uuid,
        result: ResultV3<'a>,
    },
}

#[derive(Serialize)]
struct ResultV3<'a> {
    result_id: Uuid,
    provider: &'static str,
    model: &'static str,
    language: &'static str,
    text: &'a str,
    duration_ms: u64,
    segments: Vec<SegmentV3<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_request: Option<ProviderRequestV3<'a>>,
}

#[derive(Serialize)]
struct SegmentV3<'a> {
    index: usize,
    start_ms: u64,
    end_ms: u64,
    text: &'a str,
    confidence: Option<f64>,
}

#[derive(Serialize)]
struct ProviderRequestV3<'a> {
    id: &'a str,
}

fn serialize_completed_v3(
    identity: BatchIdentity,
    job_id: Uuid,
    result: &OutboundTranscription,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&EnvelopeV3 {
        contract_version: identity.contract_version(),
        provider: identity.provider(),
        model: identity.model(),
        language: identity.language(),
        state: StateV3::Completed {
            success: true,
            status: "completed",
            job_id,
            result: ResultV3 {
                result_id: job_id,
                provider: identity.provider(),
                model: identity.model(),
                language: identity.language(),
                text: &result.text,
                duration_ms: result.duration_millis,
                segments: result
                    .segments
                    .iter()
                    .enumerate()
                    .map(|(index, segment)| SegmentV3 {
                        index,
                        start_ms: segment.start_millis,
                        end_ms: segment.end_millis,
                        text: &segment.text,
                        confidence: segment.confidence,
                    })
                    .collect(),
                provider_request: result
                    .provider_request_id
                    .as_deref()
                    .map(|id| ProviderRequestV3 { id }),
            },
        },
    })
}

fn validate_result(
    identity: BatchIdentity,
    result: &OutboundTranscription,
) -> Result<(), ContractViolation> {
    require(
        result.duration_millis <= MAX_SAFE_INTEGER,
        "duration exceeds safe integer",
    )?;
    require(
        utf16_len(&result.text) <= MAX_TRANSCRIPT_CHARS,
        "batch text exceeds bound",
    )?;
    require(result.segments.len() <= MAX_SEGMENTS, "too many segments")?;
    if identity == BatchIdentity::ElevenlabsScribeV2MultiV3 {
        require(
            result.text.is_empty() == result.segments.is_empty(),
            "text and segments disagree",
        )?;
        require(
            result.text.is_empty() || !result.text.trim().is_empty(),
            "batch text cannot contain only whitespace",
        )?;
        require(
            result.readable_segments.is_empty(),
            "v3 cannot represent readable segments",
        )?;
    } else {
        require(
            result.provider_request_id.is_none(),
            "v2 cannot represent provider request id",
        )?;
    }
    let mut previous_end = 0;
    for segment in &result.segments {
        require(
            segment.start_millis >= previous_end
                && segment.end_millis > segment.start_millis
                && segment.end_millis <= result.duration_millis,
            "invalid segment timeline",
        )?;
        require(
            !segment.text.trim().is_empty() && utf16_len(&segment.text) <= MAX_TRANSCRIPT_CHARS,
            "invalid segment text",
        )?;
        validate_confidence(segment.confidence)?;
        previous_end = segment.end_millis;
    }
    validate_provider_request(result.provider_request_id.as_deref())?;
    validate_readable(result)
}

fn validate_readable(result: &OutboundTranscription) -> Result<(), ContractViolation> {
    require(
        result.readable_segments.len() <= MAX_SEGMENTS,
        "too many readable segments",
    )?;
    let mut references = 0_usize;
    let mut characters = 0_usize;
    for segment in &result.readable_segments {
        require(
            segment.end_millis > segment.start_millis
                && segment.end_millis <= result.duration_millis,
            "invalid readable segment timeline",
        )?;
        require(
            !segment.text.trim().is_empty(),
            "invalid readable segment text",
        )?;
        let mut previous = None;
        for &index in &segment.source_segment_indices {
            require(
                index < result.segments.len() && previous.is_none_or(|value| index > value),
                "invalid readable source index",
            )?;
            previous = Some(index);
        }
        require(previous.is_some(), "readable segment has no source")?;
        references = references
            .checked_add(segment.source_segment_indices.len())
            .ok_or(ContractViolation("too many readable references"))?;
        characters = characters
            .checked_add(utf16_len(&segment.text))
            .ok_or(ContractViolation("too much readable text"))?;
    }
    require(
        references <= MAX_READABLE_REFERENCES && characters <= MAX_TRANSCRIPT_CHARS,
        "readable projection exceeds bounds",
    )
}

fn validate_provider_request(value: Option<&str>) -> Result<(), ContractViolation> {
    require(
        value.is_none_or(|id| {
            !id.is_empty()
                && utf16_len(id) <= MAX_PROVIDER_REQUEST_ID_CHARS
                && id.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        }),
        "invalid provider request id",
    )
}

fn validate_confidence(value: Option<f64>) -> Result<(), ContractViolation> {
    require(
        value.is_none_or(|confidence| confidence.is_finite() && (0.0..=1.0).contains(&confidence)),
        "invalid confidence",
    )
}

fn validate_error_code(value: &str) -> Result<(), ContractViolation> {
    require(
        !value.is_empty()
            && utf16_len(value) <= MAX_ERROR_CODE_CHARS
            && value
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'),
        "invalid error code",
    )
}

#[allow(clippy::cast_precision_loss)]
fn seconds(milliseconds: u64) -> f64 {
    milliseconds as f64 / 1_000.0
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn require(condition: bool, message: &'static str) -> Result<(), ContractViolation> {
    condition.then_some(()).ok_or(ContractViolation(message))
}
