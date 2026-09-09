//! Gateway-owned projection from application results to the exact public batch response.

use uuid::Uuid;
use voicetext_speech::application::ports::{
    BatchJobId, BatchRecognitionResult, BatchResultProjection, BatchResultProjectionFailure,
};

use super::batch::BatchIdentity;
use super::batch_outbound::{
    OutboundBatchResponse, OutboundReadableSegment, OutboundSegment, OutboundTranscription,
    serialize_response,
};

/// Exact gateway contract validator injected at the application completion boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct GatewayBatchResultProjection;

impl BatchResultProjection for GatewayBatchResultProjection {
    fn validate(
        &self,
        id: &BatchJobId,
        result: &BatchRecognitionResult,
    ) -> Result<(), BatchResultProjectionFailure> {
        let job_id = Uuid::parse_str(id.as_str()).map_err(|_| BatchResultProjectionFailure)?;
        let identity = identity_for_result(result).ok_or(BatchResultProjectionFailure)?;
        let response = completed_response(identity, job_id, result);
        serialize_response(identity, &response)
            .map(|_| ())
            .map_err(|_| BatchResultProjectionFailure)
    }
}

/// Builds the single completed projection shared by pre-persistence validation and HTTP serving.
#[must_use]
pub fn completed_response(
    identity: BatchIdentity,
    job_id: Uuid,
    result: &BatchRecognitionResult,
) -> OutboundBatchResponse {
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

fn identity_for_result(result: &BatchRecognitionResult) -> Option<BatchIdentity> {
    match (
        result.profile.contract_version(),
        result.profile.provider(),
        result.profile.model(),
        result.profile.language(),
    ) {
        (2, "deepgram", "nova-3", "multi") => Some(BatchIdentity::DeepgramNova3MultiV2),
        (3, "elevenlabs", "scribe_v2", "multi") => Some(BatchIdentity::ElevenlabsScribeV2MultiV3),
        _ => None,
    }
}
