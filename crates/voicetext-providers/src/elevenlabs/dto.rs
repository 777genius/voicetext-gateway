use serde::Deserialize;
use std::time::Duration;

const MALFORMED_RESPONSE: &str = "ELEVENLABS_MALFORMED_RESPONSE";
const MAX_TRANSCRIPT_BYTES: usize = 4 * 1_024 * 1_024;
const MAX_WORDS: usize = 100_000;
const MAX_SEGMENTS: usize = 10_000;
const MAX_TOKEN_BYTES: usize = 64 * 1_024;
const PAUSE_SPLIT_SECONDS: f64 = 0.8;
const PUNCTUATION_SPLIT_SECONDS: f64 = 12.0;
const HARD_SPLIT_SECONDS: f64 = 20.0;
const TIMESTAMP_TOLERANCE_SECONDS: f64 = 0.250;
const TIMESTAMP_RELATIVE_TOLERANCE: f64 = 0.005;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ParsedBatchResult {
    pub text: String,
    pub provider_duration_millis: Option<u64>,
    pub segments: Vec<ParsedSegment>,
    pub provider_request_id: Option<String>,
    pub provider_identity_is_transcription: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ParsedSegment {
    pub start_millis: u64,
    pub end_millis: u64,
    pub text: String,
    pub confidence: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParseFailure {
    pub code: &'static str,
    pub provider_request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseDto {
    language_code: String,
    language_probability: f64,
    text: String,
    words: Vec<WordDto>,
    #[serde(default)]
    audio_duration_secs: Option<f64>,
    #[serde(default)]
    transcription_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WordDto {
    #[serde(rename = "type")]
    kind: String,
    text: String,
    #[serde(default)]
    start: Option<f64>,
    #[serde(default)]
    end: Option<f64>,
    #[serde(default)]
    logprob: Option<f64>,
}

#[derive(Debug)]
struct SegmentBuilder {
    text: String,
    start_seconds: f64,
    end_seconds: f64,
    confidence_sum: f64,
    confidence_count: usize,
    word_count: usize,
}

impl SegmentBuilder {
    fn new(start_seconds: f64, end_seconds: f64, text: &str, confidence: Option<f64>) -> Self {
        Self {
            text: text.to_owned(),
            start_seconds,
            end_seconds,
            confidence_sum: confidence.unwrap_or_default(),
            confidence_count: usize::from(confidence.is_some()),
            word_count: 1,
        }
    }

    fn push_word(&mut self, text: &str, end_seconds: f64, confidence: Option<f64>) {
        self.text.push_str(text);
        self.end_seconds = end_seconds;
        self.word_count += 1;
        if let Some(confidence) = confidence {
            self.confidence_sum += confidence;
            self.confidence_count += 1;
        }
    }

    fn push_spacing(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn finish(self) -> Option<ParsedSegment> {
        let confidence = (self.confidence_count == self.word_count)
            .then(|| u32::try_from(self.word_count).ok())
            .flatten()
            .and_then(|word_count| {
                serde_json::from_value(serde_json::Value::from(
                    self.confidence_sum / f64::from(word_count),
                ))
                .ok()
            });
        Some(ParsedSegment {
            start_millis: seconds_to_millis(self.start_seconds).ok()?,
            end_millis: seconds_to_millis(self.end_seconds).ok()?,
            text: self.text,
            confidence,
        })
    }
}

pub(super) fn parse_response(
    bytes: &[u8],
    authoritative_duration_millis: u64,
    header_request_id: Option<String>,
) -> Result<ParsedBatchResult, ParseFailure> {
    let payload: ResponseDto =
        serde_json::from_slice(bytes).map_err(|_| malformed(header_request_id.clone()))?;
    let transcription_id = payload
        .transcription_id
        .as_deref()
        .and_then(safe_request_id);
    let provider_identity_is_transcription = transcription_id.is_some();
    let provider_request_id = transcription_id.or(header_request_id);
    validate_envelope(&payload, provider_request_id.clone())?;

    let authoritative_seconds = Duration::from_millis(authoritative_duration_millis).as_secs_f64();
    let mut reconstructed = String::new();
    let mut segments = Vec::new();
    let mut current: Option<SegmentBuilder> = None;
    let mut previous_word_end = None;

    for word in payload.words {
        if word.text.len() > MAX_TOKEN_BYTES {
            return Err(malformed(provider_request_id));
        }
        match word.kind.as_str() {
            "audio_event" => continue,
            "spacing" => {
                reconstructed.push_str(&word.text);
                if let Some(segment) = &mut current {
                    segment.push_spacing(&word.text);
                }
            }
            "word" => {
                if word.text.is_empty() {
                    return Err(malformed(provider_request_id));
                }
                let start = bounded_timestamp(
                    word.start,
                    authoritative_seconds,
                    provider_request_id.clone(),
                )?;
                let end = bounded_timestamp(
                    word.end,
                    authoritative_seconds,
                    provider_request_id.clone(),
                )?;
                if end <= start || previous_word_end.is_some_and(|previous| start < previous) {
                    return Err(malformed(provider_request_id));
                }
                let confidence = word
                    .logprob
                    .map(|value| log_probability(value, provider_request_id.clone()))
                    .transpose()?;
                let should_split = current.as_ref().is_some_and(|segment| {
                    previous_word_end
                        .is_some_and(|previous| start - previous >= PAUSE_SPLIT_SECONDS)
                        || end - segment.start_seconds > HARD_SPLIT_SECONDS
                        || (start - segment.start_seconds >= PUNCTUATION_SPLIT_SECONDS
                            && ends_sentence_punctuation(&segment.text))
                });
                if should_split {
                    push_segment(&mut segments, current.take(), provider_request_id.clone())?;
                }
                if let Some(segment) = &mut current {
                    segment.push_word(&word.text, end, confidence);
                } else {
                    current = Some(SegmentBuilder::new(start, end, &word.text, confidence));
                }
                previous_word_end = Some(end);
                reconstructed.push_str(&word.text);
            }
            _ => return Err(malformed(provider_request_id)),
        }
        if reconstructed.len() > MAX_TRANSCRIPT_BYTES {
            return Err(malformed(provider_request_id));
        }
    }
    push_segment(&mut segments, current, provider_request_id.clone())?;
    if normalize_text(&reconstructed) != normalize_text(&payload.text)
        || payload.text.len() > MAX_TRANSCRIPT_BYTES
        || (payload.text.trim().is_empty() != segments.is_empty())
    {
        return Err(malformed(provider_request_id));
    }

    let provider_duration_millis = payload
        .audio_duration_secs
        .map(seconds_to_millis)
        .transpose()
        .map_err(|()| malformed(provider_request_id.clone()))?;
    Ok(ParsedBatchResult {
        text: payload.text,
        provider_duration_millis,
        segments,
        provider_request_id,
        provider_identity_is_transcription,
    })
}

fn validate_envelope(
    payload: &ResponseDto,
    provider_request_id: Option<String>,
) -> Result<(), ParseFailure> {
    if payload.words.len() > MAX_WORDS
        || payload.language_code.trim().is_empty()
        || !payload.language_probability.is_finite()
        || !(0.0..=1.0).contains(&payload.language_probability)
    {
        return Err(malformed(provider_request_id));
    }
    Ok(())
}

fn bounded_timestamp(
    value: Option<f64>,
    authoritative_seconds: f64,
    provider_request_id: Option<String>,
) -> Result<f64, ParseFailure> {
    let value = value.ok_or_else(|| malformed(provider_request_id.clone()))?;
    let tolerance =
        TIMESTAMP_TOLERANCE_SECONDS.max(authoritative_seconds * TIMESTAMP_RELATIVE_TOLERANCE);
    if !value.is_finite() || value < 0.0 || value > authoritative_seconds + tolerance {
        return Err(malformed(provider_request_id));
    }
    Ok(value.min(authoritative_seconds))
}

fn log_probability(value: f64, provider_request_id: Option<String>) -> Result<f64, ParseFailure> {
    if !value.is_finite() || value > 0.0 {
        return Err(malformed(provider_request_id));
    }
    Ok(value.exp())
}

fn push_segment(
    segments: &mut Vec<ParsedSegment>,
    segment: Option<SegmentBuilder>,
    provider_request_id: Option<String>,
) -> Result<(), ParseFailure> {
    let Some(segment) = segment else {
        return Ok(());
    };
    if segment.text.trim().is_empty() || segments.len() == MAX_SEGMENTS {
        return Err(malformed(provider_request_id));
    }
    let segment = segment
        .finish()
        .ok_or_else(|| malformed(provider_request_id.clone()))?;
    if segment.end_millis <= segment.start_millis
        || segments
            .last()
            .is_some_and(|previous| segment.start_millis < previous.end_millis)
    {
        return Err(malformed(provider_request_id));
    }
    segments.push(segment);
    Ok(())
}

fn seconds_to_millis(seconds: f64) -> Result<u64, ()> {
    let rounded = Duration::try_from_secs_f64(seconds)
        .map_err(|_| ())?
        .checked_add(Duration::from_micros(500))
        .ok_or(())?;
    u64::try_from(rounded.as_millis()).map_err(|_| ())
}

fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ends_sentence_punctuation(value: &str) -> bool {
    value
        .trim_end()
        .ends_with(['.', '!', '?', '。', '！', '？'])
}

pub(super) fn safe_request_id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
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
    fn parses_bounded_words_timestamps_confidence_and_request_id() {
        let payload = serde_json::json!({
            "language_code": "ru", "language_probability": 0.99,
            "audio_duration_secs": 2.1, "transcription_id": "tx-123",
            "text": "Привет world!", "words": [
                {"type":"word","text":"Привет","start":0.1,"end":0.5,"logprob":-0.1},
                {"type":"spacing","text":" "},
                {"type":"word","text":"world!","start":0.5,"end":2.1,"logprob":-0.2}
            ]
        });

        let result = parse_response(payload.to_string().as_bytes(), 2_000, Some("header".into()))
            .expect("valid response");

        assert_eq!(result.text, "Привет world!");
        assert_eq!(result.provider_duration_millis, Some(2_100));
        assert_eq!(result.segments[0].end_millis, 2_000);
        assert!(
            result.segments[0]
                .confidence
                .is_some_and(|value| value > 0.8)
        );
        assert_eq!(result.provider_request_id.as_deref(), Some("tx-123"));
    }

    #[test]
    fn accepts_empty_or_event_only_transcripts() {
        for words in [
            serde_json::json!([]),
            serde_json::json!([
                {"type":"audio_event","text":"(noise)","start":0.0,"end":0.2}
            ]),
        ] {
            let payload = serde_json::json!({
                "language_code":"en", "language_probability":1.0, "text":"", "words":words
            });
            let result = parse_response(payload.to_string().as_bytes(), 1_000, None).unwrap();
            assert!(result.segments.is_empty());
        }
    }

    #[test]
    fn rejects_malformed_timeline_text_and_confidence() {
        for payload in [
            serde_json::json!({"language_code":"en","language_probability":1.0,"text":"a b","words":[
                {"type":"word","text":"a","start":0.5,"end":0.7},
                {"type":"spacing","text":" "},
                {"type":"word","text":"b","start":0.6,"end":0.8}
            ]}),
            serde_json::json!({"language_code":"en","language_probability":1.0,"text":"different","words":[
                {"type":"word","text":"a","start":0.0,"end":0.5}
            ]}),
            serde_json::json!({"language_code":"en","language_probability":1.0,"text":"a","words":[
                {"type":"word","text":"a","start":0.0,"end":0.5,"logprob":0.1}
            ]}),
        ] {
            assert!(parse_response(payload.to_string().as_bytes(), 1_000, None).is_err());
        }
        assert!(safe_request_id("bad\nid").is_none());
    }
}
