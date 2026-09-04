use serde_json::Value;
use std::time::Duration;

const MALFORMED_RESPONSE: &str = "DEEPGRAM_MALFORMED_RESPONSE";
const MAX_READABLE_SEGMENTS: usize = 10_000;
const MAX_READABLE_SEGMENT_TEXT_BYTES: usize = 64 * 1024;
const MAX_SOURCE_SEGMENTS: usize = 4_096;
const MAX_SOURCE_REFERENCES: usize = 100_000;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ParsedBatchResult {
    pub text: String,
    pub provider_duration_ms: u64,
    pub segments: Vec<ParsedSegment>,
    pub readable_segments: Vec<ParsedReadableSegment>,
    pub provider_request_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ParsedSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub confidence: Option<f32>,
    pub speaker: Option<String>,
    start_seconds: f64,
    end_seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParsedReadableSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub source_segment_indices: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParseFailure {
    pub code: &'static str,
    pub provider_request_id: Option<String>,
}

pub(super) fn parse_response(
    bytes: &[u8],
    header_request_id: Option<String>,
) -> Result<ParsedBatchResult, ParseFailure> {
    let payload: Value =
        serde_json::from_slice(bytes).map_err(|_| malformed(header_request_id.clone()))?;
    let metadata = payload
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed(header_request_id.clone()))?;
    let duration_seconds = metadata
        .get("duration")
        .and_then(Value::as_f64)
        .filter(|duration| duration.is_finite() && *duration >= 0.0)
        .ok_or_else(|| malformed(header_request_id.clone()))?;
    let provider_duration_ms =
        seconds_to_millis(duration_seconds).ok_or_else(|| malformed(header_request_id.clone()))?;
    let provider_request_id = metadata
        .get("request_id")
        .and_then(Value::as_str)
        .and_then(safe_request_id)
        .or(header_request_id);

    let alternative = payload
        .pointer("/results/channels/0/alternatives/0")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed(provider_request_id.clone()))?;
    let text = alternative
        .get("transcript")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(provider_request_id.clone()))?
        .to_owned();
    let utterances = payload
        .pointer("/results/utterances")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed(provider_request_id.clone()))?
        .iter()
        .map(|utterance| parse_utterance(utterance, provider_request_id.as_deref()))
        .collect::<Result<Vec<_>, _>>()?;
    let readable_segments =
        parse_readable_segments(&payload, &text, duration_seconds, &utterances).unwrap_or_default();

    Ok(ParsedBatchResult {
        text,
        provider_duration_ms,
        segments: utterances,
        readable_segments,
        provider_request_id,
    })
}

fn parse_utterance(
    value: &Value,
    provider_request_id: Option<&str>,
) -> Result<ParsedSegment, ParseFailure> {
    let object = value
        .as_object()
        .ok_or_else(|| malformed(provider_request_id.map(str::to_owned)))?;
    let start = finite_non_negative(object.get("start"))
        .ok_or_else(|| malformed(provider_request_id.map(str::to_owned)))?;
    let end = finite_non_negative(object.get("end"))
        .filter(|end| *end >= start)
        .ok_or_else(|| malformed(provider_request_id.map(str::to_owned)))?;
    let start_ms = seconds_to_millis(start)
        .ok_or_else(|| malformed(provider_request_id.map(str::to_owned)))?;
    let end_ms =
        seconds_to_millis(end).ok_or_else(|| malformed(provider_request_id.map(str::to_owned)))?;
    let text = object
        .get("transcript")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(provider_request_id.map(str::to_owned)))?
        .to_owned();
    let confidence = object
        .get("confidence")
        .and_then(|value| serde_json::from_value::<f32>(value.clone()).ok())
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value));
    let speaker = object
        .get("speaker")
        .and_then(Value::as_i64)
        .map(|speaker| speaker.to_string());

    Ok(ParsedSegment {
        start_ms,
        end_ms,
        text,
        confidence,
        speaker,
        start_seconds: start,
        end_seconds: end,
    })
}

fn parse_readable_segments(
    payload: &Value,
    transcript: &str,
    duration_seconds: f64,
    source_segments: &[ParsedSegment],
) -> Option<Vec<ParsedReadableSegment>> {
    let paragraphs = payload
        .pointer("/results/channels/0/alternatives/0/paragraphs/paragraphs")?
        .as_array()?;
    let sentence_groups = paragraphs
        .iter()
        .map(|paragraph| paragraph.get("sentences")?.as_array())
        .collect::<Option<Vec<_>>>()?;
    let sentence_count = sentence_groups
        .iter()
        .map(|sentences| sentences.len())
        .sum::<usize>();
    if sentence_count > MAX_READABLE_SEGMENTS || source_segments.len() > MAX_SOURCE_SEGMENTS {
        return None;
    }

    let mut result = Vec::with_capacity(sentence_count);
    let mut reference_count = 0_usize;
    let mut previous_end_seconds = None;
    for sentence in sentence_groups.into_iter().flatten() {
        let object = sentence.as_object()?;
        let start_seconds = finite_non_negative(object.get("start"))?;
        let end_seconds = finite_non_negative(object.get("end"))
            .filter(|end| *end > start_seconds && *end <= duration_seconds)?;
        if previous_end_seconds.is_some_and(|previous| start_seconds < previous) {
            return None;
        }
        previous_end_seconds = Some(end_seconds);
        let text = object.get("text")?.as_str()?;
        if text.is_empty() || text.len() > MAX_READABLE_SEGMENT_TEXT_BYTES {
            return None;
        }
        let start_ms = seconds_to_millis(start_seconds)?;
        let end_ms = seconds_to_millis(end_seconds)?;
        let source_segment_indices = source_segments
            .iter()
            .enumerate()
            .filter_map(|(index, source)| {
                (source.start_seconds < end_seconds && start_seconds < source.end_seconds)
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        if source_segment_indices.is_empty() {
            return None;
        }
        reference_count = reference_count.checked_add(source_segment_indices.len())?;
        if reference_count > MAX_SOURCE_REFERENCES {
            return None;
        }
        result.push(ParsedReadableSegment {
            start_ms,
            end_ms,
            text: text.to_owned(),
            source_segment_indices,
        });
    }

    let covered_text = result
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    (normalize_text(&covered_text) == normalize_text(transcript)).then_some(result)
}

fn finite_non_negative(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn seconds_to_millis(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let rounded = Duration::try_from_secs_f64(seconds)
        .ok()?
        .checked_add(Duration::from_micros(500))?;
    u64::try_from(rounded.as_millis()).ok()
}

fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn safe_request_id(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.trim() == value
        && value.len() <= 128
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)))
    .then(|| value.to_owned())
}

fn malformed(provider_request_id: Option<String>) -> ParseFailure {
    ParseFailure {
        code: MALFORMED_RESPONSE,
        provider_request_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_only_bounded_transcript_evidence() {
        let payload = serde_json::json!({
            "metadata": {"duration": 4.0, "request_id": "metadata-id"},
            "results": {
                "channels": [{"alternatives": [{
                    "transcript": "Hello there. General Kenobi.",
                    "paragraphs": {"paragraphs": [{"sentences": [
                        {"text": "Hello there.", "start": 0.0, "end": 1.8},
                        {"text": "General Kenobi.", "start": 1.8, "end": 4.0}
                    ]}]},
                    "ignored": "private provider data"
                }]}],
                "utterances": [
                    {"start": 0.0, "end": 2.0, "transcript": "Hello there. General", "confidence": 0.9, "speaker": 1},
                    {"start": 1.5, "end": 4.0, "transcript": "Kenobi."}
                ]
            }
        });

        let result = parse_response(payload.to_string().as_bytes(), Some("header-id".into()))
            .expect("valid response");

        assert_eq!(result.provider_request_id.as_deref(), Some("metadata-id"));
        assert_eq!(result.provider_duration_ms, 4_000);
        assert_eq!(result.segments.len(), 2);
        assert_eq!(result.segments[0].speaker.as_deref(), Some("1"));
        assert_eq!(
            result.readable_segments[0].source_segment_indices,
            vec![0, 1]
        );
        assert_eq!(
            result.readable_segments[1].source_segment_indices,
            vec![0, 1]
        );
    }

    #[test]
    fn malformed_payload_preserves_a_safe_provider_reference() {
        let payload = br#"{"metadata":{"duration":2.0,"request_id":"dg-safe"},"results":{}}"#;

        let failure = parse_response(payload, Some("header-id".into())).unwrap_err();

        assert_eq!(failure.code, MALFORMED_RESPONSE);
        assert_eq!(failure.provider_request_id.as_deref(), Some("dg-safe"));
    }

    #[test]
    fn invalid_paragraph_projection_falls_back_without_losing_utterances() {
        let payload = serde_json::json!({
            "metadata": {"duration": 2.0},
            "results": {
                "channels": [{"alternatives": [{
                    "transcript": "hello world",
                    "paragraphs": {"paragraphs": [{"sentences": [
                        {"text": "hello", "start": 0.0, "end": 1.0}
                    ]}]}
                }]}],
                "utterances": [{"start": 0.0, "end": 2.0, "transcript": "hello world"}]
            }
        });

        let result = parse_response(payload.to_string().as_bytes(), None).expect("valid result");

        assert_eq!(result.segments.len(), 1);
        assert!(result.readable_segments.is_empty());
    }

    #[test]
    fn unsafe_request_identifiers_are_discarded() {
        for rejected in [
            "",
            " request-1",
            "request-1 ",
            "bad\nid",
            "bad\tid",
            "не-ascii",
        ] {
            assert!(safe_request_id(rejected).is_none(), "accepted {rejected:?}");
        }
        assert_eq!(safe_request_id(&"x".repeat(129)), None);
    }
}
