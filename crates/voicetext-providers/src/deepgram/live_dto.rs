use std::time::Duration;

use serde_json::{Map, Value};
use url::Url;
use voicetext_speech::application::ports::{
    LiveRecognitionEvent, LiveTranscript, LiveTranscriptStability,
};

pub(super) const MAX_PROVIDER_TEXT_BYTES: usize = 256 * 1024;
const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;
const MAX_KEYTERMS: usize = 100;
const MAX_KEYTERM_BYTES: usize = 256;
const MALFORMED_RESPONSE: &str = "DEEPGRAM_LIVE_MALFORMED_RESPONSE";

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ParsedLiveMessage {
    Events {
        primary: LiveRecognitionEvent,
        follow_up: Option<LiveRecognitionEvent>,
    },
    Metadata(Option<String>),
    TerminalError,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ParseFailure {
    pub code: &'static str,
}

pub(super) fn parse_text(payload: &str) -> Result<ParsedLiveMessage, ParseFailure> {
    if payload.len() > MAX_PROVIDER_TEXT_BYTES {
        return Err(malformed());
    }
    let value: Value = serde_json::from_str(payload).map_err(|_| malformed())?;
    let object = value.as_object().ok_or_else(malformed)?;
    let message_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;

    match message_type {
        "Results" => parse_results(object),
        "UtteranceEnd" => parse_utterance_end(object),
        "Metadata" => Ok(ParsedLiveMessage::Metadata(
            object
                .get("request_id")
                .and_then(Value::as_str)
                .and_then(super::dto::safe_request_id),
        )),
        "Error" => Ok(ParsedLiveMessage::TerminalError),
        _ => Ok(ParsedLiveMessage::Ignore),
    }
}

pub(super) fn language_is_safe(language: &str) -> bool {
    (1..=10).contains(&language.len())
        && !language.starts_with('-')
        && !language.ends_with('-')
        && language
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(super) fn build_url(
    mut endpoint: Url,
    sample_rate_hz: u32,
    language: &str,
    keyterms: &[String],
) -> Url {
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    let sample_rate = sample_rate_hz.to_string();
    let mut query = endpoint.query_pairs_mut();
    for (name, value) in [
        ("encoding", "linear16"),
        ("sample_rate", sample_rate.as_str()),
        ("channels", "1"),
        ("language", language),
        ("model", "nova-3"),
        ("punctuate", "true"),
        ("interim_results", "true"),
        ("utterance_end_ms", "1400"),
        ("vad_events", "true"),
        ("endpointing", "300"),
        ("smart_format", "true"),
        ("no_delay", "true"),
    ] {
        query.append_pair(name, value);
    }
    for keyterm in keyterms
        .iter()
        .map(|term| term.trim())
        .filter(|term| !term.is_empty() && term.len() <= MAX_KEYTERM_BYTES)
        .take(MAX_KEYTERMS)
    {
        query.append_pair("keyterm", keyterm);
    }
    drop(query);
    endpoint
}

fn parse_results(object: &Map<String, Value>) -> Result<ParsedLiveMessage, ParseFailure> {
    let start = required_non_negative(object.get("start"))?;
    let duration = required_non_negative(object.get("duration"))?;
    let start_millis = seconds_to_millis(start).ok_or_else(malformed)?;
    let duration_millis = seconds_to_millis(duration).ok_or_else(malformed)?;
    let is_final = optional_bool(object.get("is_final"))?;
    let speech_final = optional_bool(object.get("speech_final"))?;
    let from_finalize = optional_bool(object.get("from_finalize"))?;
    let alternative = object
        .get("channel")
        .and_then(Value::as_object)
        .and_then(|channel| channel.get("alternatives"))
        .and_then(Value::as_array)
        .and_then(|alternatives| alternatives.first())
        .and_then(Value::as_object)
        .ok_or_else(malformed)?;
    let text = alternative
        .get("transcript")
        .and_then(Value::as_str)
        .filter(|text| text.len() <= MAX_TRANSCRIPT_BYTES)
        .ok_or_else(malformed)?
        .to_owned();
    let confidence = parse_confidence(alternative.get("confidence"))?;

    if text.is_empty() && !is_final && !speech_final && !from_finalize {
        return Ok(ParsedLiveMessage::Ignore);
    }
    let stability = if speech_final {
        LiveTranscriptStability::UtteranceFinal
    } else if is_final {
        LiveTranscriptStability::SegmentFinal
    } else {
        LiveTranscriptStability::Partial
    };
    Ok(ParsedLiveMessage::Events {
        primary: LiveRecognitionEvent::Transcript(LiveTranscript {
            text,
            start_millis,
            duration_millis,
            confidence,
            stability,
        }),
        follow_up: from_finalize.then_some(LiveRecognitionEvent::FinalizeResultObserved),
    })
}

fn parse_utterance_end(object: &Map<String, Value>) -> Result<ParsedLiveMessage, ParseFailure> {
    let last_word_end = required_non_negative(object.get("last_word_end"))?;
    let last_word_end_millis = seconds_to_millis(last_word_end).ok_or_else(malformed)?;
    Ok(ParsedLiveMessage::Events {
        primary: LiveRecognitionEvent::UtteranceEnd {
            last_word_end_millis,
        },
        follow_up: None,
    })
}

fn required_non_negative(value: Option<&Value>) -> Result<f64, ParseFailure> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number >= 0.0)
        .ok_or_else(malformed)
}

fn optional_bool(value: Option<&Value>) -> Result<bool, ParseFailure> {
    value.map_or(Ok(false), |value| value.as_bool().ok_or_else(malformed))
}

fn parse_confidence(value: Option<&Value>) -> Result<Option<f32>, ParseFailure> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value::<f32>(value.clone())
            .ok()
            .filter(|confidence| confidence.is_finite() && (0.0..=1.0).contains(confidence))
            .map(Some)
            .ok_or_else(malformed),
    }
}

fn seconds_to_millis(seconds: f64) -> Option<u64> {
    let rounded = Duration::try_from_secs_f64(seconds)
        .ok()?
        .checked_add(Duration::from_micros(500))?;
    u64::try_from(rounded.as_millis()).ok()
}

const fn malformed() -> ParseFailure {
    ParseFailure {
        code: MALFORMED_RESPONSE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_request_language_exactly_and_rejects_injection() {
        for language in ["multi", "ru", "en-US", "pt-BR"] {
            assert!(language_is_safe(language));
        }
        for language in ["", "-ru", "ru-", "ru_RU", "ru&model=x", "русский"] {
            assert!(!language_is_safe(language));
        }
        let endpoint = Url::parse("wss://api.deepgram.com/v1/listen?discarded=true").unwrap();
        let url = build_url(endpoint, 48_000, "ru", &[]);
        assert_eq!(
            url.query(),
            Some(concat!(
                "encoding=linear16&sample_rate=48000&channels=1&language=ru&model=nova-3&",
                "punctuate=true&interim_results=true&utterance_end_ms=1400&vad_events=true&",
                "endpointing=300&smart_format=true&no_delay=true"
            ))
        );
    }

    #[test]
    fn projects_partial_segment_and_utterance_final_results() {
        for (flags, expected) in [
            (
                r#""is_final":false,"speech_final":false"#,
                LiveTranscriptStability::Partial,
            ),
            (
                r#""is_final":true,"speech_final":false"#,
                LiveTranscriptStability::SegmentFinal,
            ),
            (
                r#""is_final":true,"speech_final":true"#,
                LiveTranscriptStability::UtteranceFinal,
            ),
        ] {
            let payload = format!(
                r#"{{"type":"Results","start":1.25,"duration":0.5,{flags},"channel":{{"alternatives":[{{"transcript":"hello","confidence":0.75}}]}}}}"#
            );
            let ParsedLiveMessage::Events { primary, .. } = parse_text(&payload).unwrap() else {
                panic!("expected transcript event");
            };
            let LiveRecognitionEvent::Transcript(transcript) = primary else {
                panic!("expected transcript");
            };
            assert_eq!(transcript.start_millis, 1_250);
            assert_eq!(transcript.duration_millis, 500);
            assert_eq!(transcript.confidence, Some(0.75));
            assert_eq!(transcript.stability, expected);
        }
    }

    #[test]
    fn finalize_result_carries_a_follow_up_marker() {
        let payload = r#"{"type":"Results","start":0,"duration":0.1,"is_final":true,"from_finalize":true,"channel":{"alternatives":[{"transcript":"tail"}]}}"#;
        let ParsedLiveMessage::Events { primary, follow_up } = parse_text(payload).unwrap() else {
            panic!("expected events");
        };
        assert!(matches!(primary, LiveRecognitionEvent::Transcript(_)));
        assert_eq!(
            follow_up,
            Some(LiveRecognitionEvent::FinalizeResultObserved)
        );
    }

    #[test]
    fn parses_utterance_end_and_safe_metadata() {
        assert_eq!(
            parse_text(r#"{"type":"UtteranceEnd","last_word_end":2.5}"#).unwrap(),
            ParsedLiveMessage::Events {
                primary: LiveRecognitionEvent::UtteranceEnd {
                    last_word_end_millis: 2_500,
                },
                follow_up: None,
            }
        );
        assert_eq!(
            parse_text(r#"{"type":"Metadata","request_id":" request-7 "}"#).unwrap(),
            ParsedLiveMessage::Metadata(Some("request-7".into()))
        );
    }

    #[test]
    fn rejects_invalid_timing_confidence_and_oversized_payloads() {
        for payload in [
            r#"{"type":"UtteranceEnd","last_word_end":-1}"#.to_owned(),
            r#"{"type":"Results","start":0,"duration":1,"channel":{"alternatives":[{"transcript":"x","confidence":2}]}}"#.to_owned(),
            format!(r#"{{"type":"Unknown","padding":"{}"}}"#, "x".repeat(MAX_PROVIDER_TEXT_BYTES)),
        ] {
            assert_eq!(parse_text(&payload), Err(malformed()));
        }
    }

    #[test]
    fn ignores_unknown_and_empty_partial_messages() {
        assert_eq!(
            parse_text(r#"{"type":"FutureEvent","anything":true}"#).unwrap(),
            ParsedLiveMessage::Ignore
        );
        assert_eq!(
            parse_text(r#"{"type":"Results","start":0,"duration":0,"channel":{"alternatives":[{"transcript":""}]}}"#).unwrap(),
            ParsedLiveMessage::Ignore
        );
    }
}
