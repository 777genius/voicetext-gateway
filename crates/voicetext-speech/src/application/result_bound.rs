//! One exact serialized envelope for durable batch recognition results.

use serde::Serialize;

use super::ports::{BatchReadableSegment, BatchRecognitionResult, BatchSegment};

/// `PostgreSQL`'s maximum serialized `result_json` envelope.
pub const MAX_SERIALIZED_RESULT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Serialize)]
struct ResultEnvelope<'a> {
    text: &'a str,
    duration_millis: u64,
    provider_duration_millis: Option<u64>,
    segments: Vec<SegmentEnvelope<'a>>,
    readable_segments: Option<Vec<ReadableEnvelope<'a>>>,
}

#[derive(Serialize)]
struct SegmentEnvelope<'a> {
    start_millis: u64,
    end_millis: u64,
    text: &'a str,
    confidence: Option<f32>,
    speaker: Option<&'a str>,
}

#[derive(Serialize)]
struct ReadableEnvelope<'a> {
    start_millis: u64,
    end_millis: u64,
    text: &'a str,
    source_segment_indices: &'a [usize],
}

/// Returns the exact JSON byte length written to `PostgreSQL`.
///
/// A serialization error is treated as outside the accepted envelope.
pub fn serialized_result_bytes(result: &BatchRecognitionResult) -> Option<usize> {
    serde_json::to_vec(&ResultEnvelope {
        text: &result.text,
        duration_millis: result.duration_millis,
        provider_duration_millis: result.provider_duration_millis,
        segments: result.segments.iter().map(segment).collect(),
        readable_segments: result
            .readable_segments
            .as_ref()
            .map(|segments| segments.iter().map(readable).collect()),
    })
    .ok()
    .map(|bytes| bytes.len())
}

/// Checks the shared durable serialized-result envelope.
pub fn serialized_result_fits(result: &BatchRecognitionResult) -> bool {
    serialized_result_bytes(result).is_some_and(|bytes| bytes <= MAX_SERIALIZED_RESULT_BYTES)
}

fn segment(value: &BatchSegment) -> SegmentEnvelope<'_> {
    SegmentEnvelope {
        start_millis: value.start_millis,
        end_millis: value.end_millis,
        text: &value.text,
        confidence: value.confidence,
        speaker: value.speaker.as_deref(),
    }
}

fn readable(value: &BatchReadableSegment) -> ReadableEnvelope<'_> {
    ReadableEnvelope {
        start_millis: value.start_millis,
        end_millis: value.end_millis,
        text: &value.text,
        source_segment_indices: &value.source_segment_indices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::BatchRecognitionResult;
    use crate::domain::batch::BatchProfile;

    fn result(text: String) -> BatchRecognitionResult {
        BatchRecognitionResult {
            profile: BatchProfile::new(2, "provider", "model", "multi").unwrap(),
            text,
            duration_millis: 0,
            provider_duration_millis: None,
            segments: Vec::new(),
            readable_segments: None,
            provider_reference: None,
        }
    }

    #[test]
    fn accepts_exact_boundary_and_rejects_one_byte_over() {
        let empty = result(String::new());
        let overhead = serialized_result_bytes(&empty).unwrap();
        let exact = result("a".repeat(MAX_SERIALIZED_RESULT_BYTES - overhead));
        assert_eq!(
            serialized_result_bytes(&exact),
            Some(MAX_SERIALIZED_RESULT_BYTES)
        );
        assert!(serialized_result_fits(&exact));

        let over = result("a".repeat(MAX_SERIALIZED_RESULT_BYTES - overhead + 1));
        assert_eq!(
            serialized_result_bytes(&over),
            Some(MAX_SERIALIZED_RESULT_BYTES + 1)
        );
        assert!(!serialized_result_fits(&over));
    }
}
