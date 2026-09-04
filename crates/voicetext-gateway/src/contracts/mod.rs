//! Versioned `VoiceText` boundary data and wire validation.

pub mod batch;
pub mod batch_outbound;
pub mod batch_projection;
pub mod live;
pub mod live_outbound;

use std::fmt;

/// A wire payload violated the published `VoiceText` compatibility contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContractViolation(pub &'static str);

impl fmt::Display for ContractViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ContractViolation {}

#[cfg(test)]
mod tests {
    use super::{batch, live};

    const ID: &str = "123e4567-e89b-42d3-a456-426614174000";

    #[test]
    fn batch_v2_completed_golden_json() {
        let json = format!(
            r#"{{"success":true,"status":"completed","job_id":"{ID}","result":{{"provider":"deepgram","model":"nova-3","language":"multi","text":"hello","duration_seconds":1.5,"utterances":[{{"start":0,"end":1.5,"transcript":"hello","confidence":0.9}}],"readable_segments":[{{"start":0,"end":1.5,"transcript":"hello","source_utterance_indices":[0]}}]}}}}"#
        );
        let result = batch::parse_response(
            &json,
            batch::BatchResponseStatus::Ok,
            batch::BatchIdentity::DeepgramNova3MultiV2,
        )
        .unwrap();
        assert!(
            matches!(result, batch::BatchTaskResult::Completed { result, .. } if result.utterances.len() == 1)
        );
    }

    #[test]
    fn batch_v3_pending_golden_json() {
        let json = format!(
            r#"{{"contract_version":3,"provider":"elevenlabs","model":"scribe_v2","language":"multi","success":true,"status":"running","job_id":"{ID}","next_action":"poll","retry_after_ms":1000}}"#
        );
        assert!(matches!(
            batch::parse_response(
                &json,
                batch::BatchResponseStatus::Accepted,
                batch::BatchIdentity::ElevenlabsScribeV2MultiV3
            )
            .unwrap(),
            batch::BatchTaskResult::Pending {
                retry_after_ms: 1000,
                ..
            }
        ));
    }

    #[test]
    fn batch_v3_completed_and_failed_golden_json() {
        let completed = format!(
            r#"{{"contract_version":3,"provider":"elevenlabs","model":"scribe_v2","language":"multi","success":true,"status":"completed","job_id":"{ID}","result":{{"result_id":"{ID}","provider":"elevenlabs","model":"scribe_v2","language":"multi","text":"hello","duration_ms":20,"segments":[{{"index":0,"start_ms":0,"end_ms":20,"text":"hello","confidence":null}}],"provider_request":{{"id":"request-1"}}}}}}"#
        );
        assert!(
            matches!(batch::parse_response(&completed, batch::BatchResponseStatus::Ok, batch::BatchIdentity::ElevenlabsScribeV2MultiV3).unwrap(), batch::BatchTaskResult::Completed { result, .. } if result.provider_request_id.as_deref() == Some("request-1"))
        );
        let failed = format!(
            r#"{{"contract_version":3,"provider":"elevenlabs","model":"scribe_v2","language":"multi","success":false,"status":"failed","job_id":"{ID}","retryable":false,"error_code":"PROVIDER_FAILED"}}"#
        );
        assert!(matches!(
            batch::parse_response(
                &failed,
                batch::BatchResponseStatus::Ok,
                batch::BatchIdentity::ElevenlabsScribeV2MultiV3
            )
            .unwrap(),
            batch::BatchTaskResult::Failed { .. }
        ));
    }

    #[test]
    fn batch_rejects_identity_result_id_and_bounds() {
        let mismatch = format!(
            r#"{{"contract_version":3,"provider":"deepgram","model":"scribe_v2","language":"multi","success":true,"status":"completed","job_id":"{ID}","result":{{"result_id":"123e4567-e89b-42d3-a456-426614174001","provider":"elevenlabs","model":"scribe_v2","language":"multi","text":"x","duration_ms":1,"segments":[{{"index":0,"start_ms":0,"end_ms":1,"text":"x"}}]}}}}"#
        );
        assert!(
            batch::parse_response(
                &mismatch,
                batch::BatchResponseStatus::Ok,
                batch::BatchIdentity::ElevenlabsScribeV2MultiV3
            )
            .is_err()
        );
        let long_code = "A".repeat(129);
        let failed = format!(
            r#"{{"success":false,"status":"failed","job_id":"{ID}","retryable":false,"error_code":"{long_code}"}}"#
        );
        assert!(
            batch::parse_response(
                &failed,
                batch::BatchResponseStatus::Ok,
                batch::BatchIdentity::DeepgramNova3MultiV2
            )
            .is_err()
        );
        let bad_id = r#"{"success":true,"status":"running","job_id":"not-a-uuid","next_action":"poll","retry_after_ms":1}"#;
        assert!(
            batch::parse_response(
                bad_id,
                batch::BatchResponseStatus::Accepted,
                batch::BatchIdentity::DeepgramNova3MultiV2
            )
            .is_err()
        );
        let retryable = format!(
            r#"{{"success":false,"status":"failed","job_id":"{ID}","retryable":true,"error_code":"FAILED"}}"#
        );
        assert!(
            batch::parse_response(
                &retryable,
                batch::BatchResponseStatus::Ok,
                batch::BatchIdentity::DeepgramNova3MultiV2
            )
            .is_err()
        );
    }

    #[test]
    fn live_config_round_trips_golden_json() {
        let json = format!(
            r#"{{"type":"config","provider":"deepgram","model":"nova-3","language":"multi","capabilities":["finalize_ack"],"channels":1,"protocol_v":2,"client_session_id":"{ID}","encoding":"opus","sample_rate":48000,"keyterms":["VoiceText"]}}"#
        );
        let config = live::parse_client_config(&json).unwrap();
        assert_eq!(config.identity, live::LiveIdentity::DeepgramNova3);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &live::serialize_client_config(&config).unwrap()
            )
            .unwrap(),
            serde_json::from_str::<serde_json::Value>(&json).unwrap()
        );
    }

    #[test]
    fn live_projects_segment_final_and_rejects_identity_bounds() {
        let segment = r#"{"type":"partial","text":"hello","start_ms":0,"duration_ms":20,"confidence":0.8,"is_segment_final":true}"#;
        assert!(matches!(
            live::parse_server_message(segment, live::LiveIdentity::DeepgramNova3, 100).unwrap(),
            live::ServerMessage::SegmentFinal(_)
        ));
        let ready = format!(
            r#"{{"type":"ready","session_id":"{ID}","provider":"elevenlabs","model":"scribe_v2_realtime"}}"#
        );
        assert!(
            live::parse_server_message(&ready, live::LiveIdentity::DeepgramNova3, 100).is_err()
        );
        let term = "x".repeat(257);
        let config = format!(
            r#"{{"type":"config","provider":"deepgram","model":"nova-3","language":"multi","capabilities":["finalize_ack"],"channels":1,"protocol_v":2,"client_session_id":"{ID}","encoding":"opus","sample_rate":48000,"keyterms":["{term}"]}}"#
        );
        assert!(live::parse_client_config(&config).is_err());
    }

    #[test]
    fn live_rejects_invalid_finalize_and_json_audio() {
        for json in [
            r#"{"type":"finalize_complete","status":"done","saw_result":true}"#,
            r#"{"type":"finalize_complete","status":"flushed","saw_result":false}"#,
            r#"{"type":"finalize_complete","status":"no_provider","saw_result":true}"#,
            r#"{"type":"audio","data":"base64"}"#,
        ] {
            assert!(
                live::parse_server_message(json, live::LiveIdentity::DeepgramNova3, 100).is_err()
            );
        }
    }

    #[test]
    fn transport_parsing_preserves_profile_features_for_application_validation() {
        let too_long = "x".repeat(21);
        let elevenlabs = format!(
            r#"{{"type":"config","provider":"elevenlabs","model":"scribe_v2_realtime","language":"multi","capabilities":["finalize_ack"],"channels":1,"protocol_v":2,"client_session_id":"{ID}","encoding":"opus","sample_rate":48000,"keyterms":["{too_long}"]}}"#
        );
        assert!(live::parse_client_config(&elevenlabs).is_ok());
        let deepgram = format!(
            r#"{{"type":"config","provider":"deepgram","model":"nova-3","language":"-en","capabilities":["finalize_ack"],"channels":1,"protocol_v":2,"client_session_id":"{ID}","encoding":"opus","sample_rate":48000}}"#
        );
        assert!(live::parse_client_config(&deepgram).is_ok());
    }
}

#[cfg(test)]
mod outbound_batch_tests {
    use uuid::Uuid;

    use super::batch::{
        BatchIdentity, BatchResponseStatus, BatchTaskResult, NextAction, parse_response,
    };
    use super::batch_outbound::{
        OutboundBatchResponse, OutboundReadableSegment, OutboundSegment, OutboundTranscription,
        serialize_response,
    };

    fn id() -> Uuid {
        Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap()
    }

    fn result() -> OutboundTranscription {
        OutboundTranscription {
            text: "hello".into(),
            duration_millis: 1_500,
            segments: vec![OutboundSegment {
                start_millis: 0,
                end_millis: 1_500,
                text: "hello".into(),
                confidence: Some(0.9),
            }],
            readable_segments: Vec::new(),
            provider_request_id: None,
        }
    }

    #[test]
    fn v2_pending_and_completed_are_exact_and_parseable() {
        let pending = serialize_response(
            BatchIdentity::DeepgramNova3MultiV2,
            &OutboundBatchResponse::Pending {
                job_id: id(),
                next_action: NextAction::Retry,
                retry_after_ms: 250,
            },
        )
        .unwrap();
        assert_eq!(pending.status, BatchResponseStatus::Accepted);
        assert_eq!(
            pending.body,
            r#"{"success":true,"status":"running","job_id":"123e4567-e89b-42d3-a456-426614174000","next_action":"retry","retry_after_ms":250}"#
        );
        assert!(matches!(
            parse_response(
                &pending.body,
                pending.status,
                BatchIdentity::DeepgramNova3MultiV2
            )
            .unwrap(),
            BatchTaskResult::Pending { .. }
        ));
        let mut transcription = result();
        transcription
            .readable_segments
            .push(OutboundReadableSegment {
                start_millis: 0,
                end_millis: 1_500,
                text: "hello".into(),
                source_segment_indices: vec![0],
            });
        let completed = serialize_response(
            BatchIdentity::DeepgramNova3MultiV2,
            &OutboundBatchResponse::Completed {
                job_id: id(),
                result: transcription,
            },
        )
        .unwrap();
        assert!(matches!(
            parse_response(
                &completed.body,
                completed.status,
                BatchIdentity::DeepgramNova3MultiV2
            )
            .unwrap(),
            BatchTaskResult::Completed { .. }
        ));
        assert!(completed.body.contains(r#""source_utterance_indices":[0]"#));
        let failed = serialize_response(
            BatchIdentity::DeepgramNova3MultiV2,
            &OutboundBatchResponse::Failed {
                job_id: id(),
                error_code: "PROVIDER_FAILED".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            parse_response(
                &failed.body,
                failed.status,
                BatchIdentity::DeepgramNova3MultiV2
            )
            .unwrap(),
            BatchTaskResult::Failed { .. }
        ));
    }

    #[test]
    fn v3_completed_and_failed_are_exact_and_parseable() {
        let pending = serialize_response(
            BatchIdentity::ElevenlabsScribeV2MultiV3,
            &OutboundBatchResponse::Pending {
                job_id: id(),
                next_action: NextAction::Poll,
                retry_after_ms: 500,
            },
        )
        .unwrap();
        assert_eq!(pending.status, BatchResponseStatus::Accepted);
        assert!(matches!(
            parse_response(
                &pending.body,
                pending.status,
                BatchIdentity::ElevenlabsScribeV2MultiV3
            )
            .unwrap(),
            BatchTaskResult::Pending { .. }
        ));
        let mut transcription = result();
        transcription.provider_request_id = Some("request-1".into());
        let completed = serialize_response(
            BatchIdentity::ElevenlabsScribeV2MultiV3,
            &OutboundBatchResponse::Completed {
                job_id: id(),
                result: transcription,
            },
        )
        .unwrap();
        assert_eq!(
            completed.body,
            r#"{"contract_version":3,"provider":"elevenlabs","model":"scribe_v2","language":"multi","success":true,"status":"completed","job_id":"123e4567-e89b-42d3-a456-426614174000","result":{"result_id":"123e4567-e89b-42d3-a456-426614174000","provider":"elevenlabs","model":"scribe_v2","language":"multi","text":"hello","duration_ms":1500,"segments":[{"index":0,"start_ms":0,"end_ms":1500,"text":"hello","confidence":0.9}],"provider_request":{"id":"request-1"}}}"#
        );
        assert!(matches!(
            parse_response(
                &completed.body,
                completed.status,
                BatchIdentity::ElevenlabsScribeV2MultiV3
            )
            .unwrap(),
            BatchTaskResult::Completed { .. }
        ));
        let failed = serialize_response(
            BatchIdentity::ElevenlabsScribeV2MultiV3,
            &OutboundBatchResponse::Failed {
                job_id: id(),
                error_code: "PROVIDER_FAILED".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            parse_response(
                &failed.body,
                failed.status,
                BatchIdentity::ElevenlabsScribeV2MultiV3
            )
            .unwrap(),
            BatchTaskResult::Failed { .. }
        ));
    }

    #[test]
    fn outbound_batch_rejects_invalid_values() {
        let mut invalid = result();
        invalid.segments[0].confidence = Some(f64::NAN);
        assert!(
            serialize_response(
                BatchIdentity::DeepgramNova3MultiV2,
                &OutboundBatchResponse::Completed {
                    job_id: id(),
                    result: invalid
                }
            )
            .is_err()
        );
        let mut v3 = result();
        v3.readable_segments.push(OutboundReadableSegment {
            start_millis: 0,
            end_millis: 1,
            text: "x".into(),
            source_segment_indices: vec![0],
        });
        assert!(
            serialize_response(
                BatchIdentity::ElevenlabsScribeV2MultiV3,
                &OutboundBatchResponse::Completed {
                    job_id: id(),
                    result: v3
                }
            )
            .is_err()
        );
    }
}
