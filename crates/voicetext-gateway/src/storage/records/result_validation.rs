//! Bounded validation for normalized batch results stored by the `PostgreSQL` adapter.

use voicetext_speech::application::ports::BatchRecognitionResult;

use super::{MAX_PROVIDER_REFERENCE_BYTES, RecordError};

const MAX_SEGMENTS: usize = 10_000;
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_READABLE_REFERENCES: usize = 100_000;

pub(super) fn validate_result(result: &BatchRecognitionResult) -> Result<(), RecordError> {
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
