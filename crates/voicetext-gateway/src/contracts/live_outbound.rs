//! Strict live client controls and validated server JSON projection.

use serde::Serialize;
use uuid::{Uuid, Variant};

use super::ContractViolation;
pub use super::live::{ClientCommand, parse_client_command};
use super::live::{
    FinalizeStatus, LiveIdentity, Model, Provider, TranscriptSegment, identity_wire,
};

const MAX_ERROR_CODE_CHARS: usize = 128;
const MAX_ERROR_MESSAGE_CHARS: usize = 2_048;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TRANSCRIPT_CHARS: usize = 65_536;

/// Exact server messages emitted by the runnable live protocol adapter.
#[derive(Clone, Debug, PartialEq)]
pub enum OutboundServerMessage {
    Ready {
        session_id: Uuid,
        identity: LiveIdentity,
    },
    Ack {
        seq: u64,
    },
    Partial(TranscriptSegment),
    Final(TranscriptSegment),
    SegmentFinal(TranscriptSegment),
    FinalizeComplete {
        status: FinalizeStatus,
        saw_result: bool,
    },
    Error {
        code: String,
        message: String,
    },
}

/// Validates and serializes one exact live server text frame.
///
/// # Errors
///
/// Returns [`ContractViolation`] for invalid UUIDs, sequence numbers, transcript values, finalize
/// evidence, error bounds, non-finite confidence, or serialization failure.
pub fn serialize_server_message(
    message: &OutboundServerMessage,
) -> Result<String, ContractViolation> {
    let serialized = match message {
        OutboundServerMessage::Ready {
            session_id,
            identity,
        } => {
            validate_session_id(*session_id)?;
            let (provider, model) = identity_wire(*identity);
            serde_json::to_string(&ReadyWire {
                message_type: "ready",
                session_id,
                provider,
                model,
            })
        }
        OutboundServerMessage::Ack { seq } => {
            require(
                (1..=MAX_SAFE_INTEGER).contains(seq),
                "invalid audio acknowledgement",
            )?;
            serde_json::to_string(&AckWire {
                message_type: "ack",
                seq: *seq,
            })
        }
        OutboundServerMessage::Partial(segment) => {
            validate_segment(segment)?;
            serialize_segment("partial", segment, None)
        }
        OutboundServerMessage::Final(segment) => {
            validate_segment(segment)?;
            serialize_segment("final", segment, None)
        }
        OutboundServerMessage::SegmentFinal(segment) => {
            validate_segment(segment)?;
            serialize_segment("partial", segment, Some(true))
        }
        OutboundServerMessage::FinalizeComplete { status, saw_result } => {
            validate_finalize(*status, *saw_result)?;
            serde_json::to_string(&FinalizeWire {
                message_type: "finalize_complete",
                status: *status,
                saw_result: *saw_result,
            })
        }
        OutboundServerMessage::Error { code, message } => {
            require(
                !code.is_empty() && utf16_len(code) <= MAX_ERROR_CODE_CHARS,
                "invalid live error code",
            )?;
            require(
                utf16_len(message) <= MAX_ERROR_MESSAGE_CHARS,
                "live error message exceeds bound",
            )?;
            serde_json::to_string(&ErrorWire {
                message_type: "error",
                code,
                message,
            })
        }
    };
    serialized.map_err(|_| ContractViolation("cannot serialize live server message"))
}

#[derive(Serialize)]
struct ReadyWire<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    session_id: &'a Uuid,
    provider: Provider,
    model: Model,
}

#[derive(Serialize)]
struct AckWire {
    #[serde(rename = "type")]
    message_type: &'static str,
    seq: u64,
}

#[derive(Serialize)]
struct SegmentWire<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    text: &'a str,
    start_ms: u64,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_segment_final: Option<bool>,
}

#[derive(Serialize)]
struct FinalizeWire {
    #[serde(rename = "type")]
    message_type: &'static str,
    status: FinalizeStatus,
    saw_result: bool,
}

#[derive(Serialize)]
struct ErrorWire<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    code: &'a str,
    message: &'a str,
}

fn serialize_segment(
    message_type: &'static str,
    segment: &TranscriptSegment,
    is_segment_final: Option<bool>,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&SegmentWire {
        message_type,
        text: &segment.text,
        start_ms: segment.start_ms,
        duration_ms: segment.duration_ms,
        confidence: segment.confidence,
        is_segment_final,
    })
}

fn validate_segment(segment: &TranscriptSegment) -> Result<(), ContractViolation> {
    require(
        utf16_len(&segment.text) <= MAX_TRANSCRIPT_CHARS,
        "live transcript exceeds bound",
    )?;
    require(
        segment.start_ms <= MAX_SAFE_INTEGER && segment.duration_ms <= MAX_SAFE_INTEGER,
        "live segment time exceeds safe integer",
    )?;
    require(
        segment.start_ms.checked_add(segment.duration_ms).is_some(),
        "live segment timing overflow",
    )?;
    require(
        segment
            .confidence
            .is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value)),
        "invalid live confidence",
    )
}

fn validate_finalize(status: FinalizeStatus, saw_result: bool) -> Result<(), ContractViolation> {
    require(
        !matches!(
            (status, saw_result),
            (FinalizeStatus::Flushed, false) | (FinalizeStatus::NoProvider, true)
        ),
        "inconsistent finalize evidence",
    )
}

fn validate_session_id(session_id: Uuid) -> Result<(), ContractViolation> {
    require(
        session_id.get_variant() == Variant::RFC4122
            && (1..=8).contains(&session_id.get_version_num()),
        "invalid live session UUID",
    )
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn require(condition: bool, message: &'static str) -> Result<(), ContractViolation> {
    condition.then_some(()).ok_or(ContractViolation(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::live::{
        ClientCommand, ServerMessage, parse_client_command, parse_server_message,
    };

    fn id() -> Uuid {
        Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap()
    }

    fn segment() -> TranscriptSegment {
        TranscriptSegment {
            text: "hello".into(),
            start_ms: 10,
            duration_ms: 20,
            confidence: Some(0.9),
        }
    }

    #[test]
    fn strict_client_commands_are_exact() {
        assert_eq!(
            parse_client_command(r#"{"type":"finalize"}"#).unwrap(),
            ClientCommand::Finalize
        );
        assert_eq!(
            parse_client_command(r#"{"type":"close"}"#).unwrap(),
            ClientCommand::Close
        );
        assert!(parse_client_command(r#"{"type":"finalize","extra":true}"#).is_err());
        assert!(parse_client_command(r#"{"type":"audio","data":"x"}"#).is_err());
    }

    #[test]
    fn ready_ack_and_segments_round_trip_inbound() {
        let cases = [
            OutboundServerMessage::Ready {
                session_id: id(),
                identity: LiveIdentity::DeepgramNova3,
            },
            OutboundServerMessage::Ack { seq: 1 },
            OutboundServerMessage::Partial(segment()),
            OutboundServerMessage::Final(segment()),
            OutboundServerMessage::SegmentFinal(segment()),
        ];
        for message in cases {
            let json = serialize_server_message(&message).unwrap();
            let parsed =
                parse_server_message(&json, LiveIdentity::DeepgramNova3, MAX_TRANSCRIPT_CHARS)
                    .unwrap();
            assert!(matches!(
                (message, parsed),
                (
                    OutboundServerMessage::Ready { .. },
                    ServerMessage::Ready { .. }
                ) | (OutboundServerMessage::Ack { .. }, ServerMessage::Ack { .. })
                    | (
                        OutboundServerMessage::Partial(_),
                        ServerMessage::Partial { .. }
                    )
                    | (OutboundServerMessage::Final(_), ServerMessage::Final(_))
                    | (
                        OutboundServerMessage::SegmentFinal(_),
                        ServerMessage::SegmentFinal(_)
                    )
            ));
        }
    }

    #[test]
    fn finalize_complete_and_error_golden_json() {
        let complete = OutboundServerMessage::FinalizeComplete {
            status: FinalizeStatus::Flushed,
            saw_result: true,
        };
        assert_eq!(
            serialize_server_message(&complete).unwrap(),
            r#"{"type":"finalize_complete","status":"flushed","saw_result":true}"#
        );
        let error = OutboundServerMessage::Error {
            code: "FAILED".into(),
            message: "try later".into(),
        };
        assert_eq!(
            serialize_server_message(&error).unwrap(),
            r#"{"type":"error","code":"FAILED","message":"try later"}"#
        );
    }

    #[test]
    fn rejects_non_finite_unbounded_and_inconsistent_values() {
        let mut invalid = segment();
        invalid.confidence = Some(f64::NAN);
        assert!(serialize_server_message(&OutboundServerMessage::Final(invalid)).is_err());
        let long = "x".repeat(MAX_TRANSCRIPT_CHARS + 1);
        assert!(
            serialize_server_message(&OutboundServerMessage::Partial(TranscriptSegment {
                text: long,
                ..segment()
            }))
            .is_err()
        );
        assert!(
            serialize_server_message(&OutboundServerMessage::FinalizeComplete {
                status: FinalizeStatus::Flushed,
                saw_result: false
            })
            .is_err()
        );
    }
}
