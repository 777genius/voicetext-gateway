use voicetext_speech::application::ports::{ProviderReference, RecognitionFailure};

use super::dto::ParsedBatchResult;

const TIMESTAMP_TOLERANCE_MILLIS: u64 = 250;
const TIMESTAMP_RELATIVE_TOLERANCE_DIVISOR: u64 = 200;
const MAX_UTTERANCE_OVERLAP_MILLIS: u64 = 10_000;

pub(super) fn normalize(
    mut parsed: ParsedBatchResult,
    authoritative_duration_millis: u64,
) -> Result<ParsedBatchResult, RecognitionFailure> {
    let tolerance = TIMESTAMP_TOLERANCE_MILLIS
        .max(authoritative_duration_millis / TIMESTAMP_RELATIVE_TOLERANCE_DIVISOR);
    let maximum_provider_end = authoritative_duration_millis.saturating_add(tolerance);
    let provider_reference = parsed
        .provider_request_id
        .clone()
        .map(ProviderReference::new);
    for segment in &mut parsed.segments {
        normalize_end(
            segment.start_ms,
            &mut segment.end_ms,
            authoritative_duration_millis,
            maximum_provider_end,
            provider_reference.clone(),
        )?;
    }
    for segment in &mut parsed.readable_segments {
        normalize_end(
            segment.start_ms,
            &mut segment.end_ms,
            authoritative_duration_millis,
            maximum_provider_end,
            provider_reference.clone(),
        )?;
    }
    if normalize_utterance_overlaps(&mut parsed.segments, provider_reference)? {
        // Readable segments are optional derived metadata. Their source indices
        // no longer identify the normalized raw timeline after a merge, so
        // discard the complete projection instead of exposing stale evidence.
        parsed.readable_segments.clear();
    }
    Ok(parsed)
}

fn normalize_utterance_overlaps(
    segments: &mut Vec<super::dto::ParsedSegment>,
    provider_reference: Option<ProviderReference>,
) -> Result<bool, RecognitionFailure> {
    let mut normalized = Vec::with_capacity(segments.len());
    let mut changed = false;
    for mut segment in segments.drain(..) {
        let Some(previous) = normalized.last_mut() else {
            normalized.push(segment);
            continue;
        };
        if segment.start_ms >= previous.end_ms {
            normalized.push(segment);
            continue;
        }
        let overlap_millis = previous.end_ms - segment.start_ms;
        if overlap_millis > MAX_UTTERANCE_OVERLAP_MILLIS {
            return Err(invalid_timing(provider_reference));
        }
        changed = true;
        if segment.end_ms <= previous.end_ms {
            previous.text = merge_contained_text(&previous.text, &segment.text);
            continue;
        }
        segment.start_ms = previous.end_ms;
        normalized.push(segment);
    }
    *segments = normalized;
    Ok(changed)
}

fn merge_contained_text(previous: &str, current: &str) -> String {
    let previous_words = previous.split_whitespace().collect::<Vec<_>>();
    let current_words = current.split_whitespace().collect::<Vec<_>>();
    if contains_words(&previous_words, &current_words) {
        return previous.to_owned();
    }
    if contains_words(&current_words, &previous_words) {
        return current.to_owned();
    }
    let maximum_shared_words = previous_words.len().min(current_words.len());
    for shared_words in (1..=maximum_shared_words).rev() {
        if words_equal(
            &previous_words[previous_words.len() - shared_words..],
            &current_words[..shared_words],
        ) {
            return previous_words
                .iter()
                .chain(current_words[shared_words..].iter())
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
        }
    }
    format!("{previous} {current}")
}

fn contains_words(haystack: &[&str], needle: &[&str]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| words_equal(window, needle))
}

fn words_equal(left: &[&str], right: &[&str]) -> bool {
    left.iter()
        .zip(right)
        .all(|(left, right)| left.to_lowercase() == right.to_lowercase())
}

fn invalid_timing(provider_reference: Option<ProviderReference>) -> RecognitionFailure {
    RecognitionFailure::UnknownAfterSend {
        code: "DEEPGRAM_INVALID_TIMING".into(),
        provider_reference,
    }
}

fn normalize_end(
    start_millis: u64,
    end_millis: &mut u64,
    authoritative_duration_millis: u64,
    maximum_provider_end: u64,
    provider_reference: Option<ProviderReference>,
) -> Result<(), RecognitionFailure> {
    if start_millis >= authoritative_duration_millis
        || *end_millis > maximum_provider_end
        || *end_millis <= start_millis
    {
        return Err(invalid_timing(provider_reference));
    }
    *end_millis = (*end_millis).min(authoritative_duration_millis);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deepgram::dto;

    #[test]
    fn clamps_bounded_provider_drift_and_rejects_unbounded_timing() {
        let parsed = dto::parse_response(&timeline_body(2.6), None).unwrap();
        let normalized = normalize(parsed, 2_500).unwrap();
        assert_eq!(normalized.segments[0].end_ms, 2_500);
        assert_eq!(normalized.readable_segments[0].end_ms, 2_500);

        let parsed = dto::parse_response(&timeline_body(3.0), None).unwrap();
        let failure = normalize(parsed, 2_500).unwrap_err();
        assert_eq!(failure.code(), "DEEPGRAM_INVALID_TIMING");
    }

    #[test]
    fn normalizes_the_observed_nova_three_overlap_and_discards_stale_readable_segments() {
        let payload = serde_json::json!({
            "metadata": {"duration": 84.6, "request_id": "overlap-request"},
            "results": {
                "channels": [{"alternatives": [{
                    "transcript": "first utterance second utterance",
                    "paragraphs": {"paragraphs": [{"sentences": [{
                        "text": "first utterance second utterance",
                        "start": 75.865,
                        "end": 81.975
                    }]}]}
                }]}],
                "utterances": [
                    {"start": 75.865, "end": 80.505, "transcript": "first utterance"},
                    {"start": 79.975, "end": 81.975, "transcript": "second utterance"}
                ]
            }
        });
        let parsed = dto::parse_response(payload.to_string().as_bytes(), None).unwrap();

        let normalized = normalize(parsed, 84_600).unwrap();

        assert_eq!(normalized.segments[0].end_ms, 80_505);
        assert_eq!(normalized.segments[1].start_ms, 80_505);
        assert_eq!(normalized.segments[1].end_ms, 81_975);
        assert!(normalized.readable_segments.is_empty());
    }

    #[test]
    fn merges_a_bounded_fully_contained_refinement() {
        let parsed = dto::parse_response(
            &overlap_body(
                (10.0, 20.0, "release Redis queue"),
                (12.0, 18.0, "Redis queue"),
            ),
            None,
        )
        .unwrap();

        let normalized = normalize(parsed, 30_000).unwrap();

        assert_eq!(normalized.segments.len(), 1);
        assert_eq!(normalized.segments[0].text, "release Redis queue");
    }

    #[test]
    fn rejects_an_overlap_above_the_compatibility_bound() {
        let parsed = dto::parse_response(
            &overlap_body((0.0, 20.0, "first"), (5.0, 25.0, "second")),
            None,
        )
        .unwrap();

        let failure = normalize(parsed, 30_000).unwrap_err();

        assert_eq!(failure.code(), "DEEPGRAM_INVALID_TIMING");
    }

    fn overlap_body(first: (f64, f64, &str), second: (f64, f64, &str)) -> Vec<u8> {
        serde_json::json!({
            "metadata": {"duration": 30.0, "request_id": "overlap-request"},
            "results": {
                "channels": [{"alternatives": [{
                    "transcript": format!("{} {}", first.2, second.2)
                }]}],
                "utterances": [
                    {"start": first.0, "end": first.1, "transcript": first.2},
                    {"start": second.0, "end": second.1, "transcript": second.2}
                ]
            }
        })
        .to_string()
        .into_bytes()
    }

    fn timeline_body(end: f64) -> Vec<u8> {
        serde_json::json!({
            "metadata": {"duration": end, "request_id": "timeline-request"},
            "results": {
                "channels": [{"alternatives": [{
                    "transcript": "release train",
                    "paragraphs": {"paragraphs": [{"sentences": [{
                        "text": "release train", "start": 0.0, "end": end
                    }]}]}
                }]}],
                "utterances": [{
                    "start": 0.0, "end": end, "transcript": "release train"
                }]
            }
        })
        .to_string()
        .into_bytes()
    }
}
