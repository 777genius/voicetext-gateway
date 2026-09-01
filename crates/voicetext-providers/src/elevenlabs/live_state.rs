use std::collections::VecDeque;

use voicetext_speech::application::ports::{
    LiveRecognitionEvent, LiveTranscript, LiveTranscriptStability, ProviderReference,
};

use super::live_dto::{ParsedLiveMessage, TranscriptKind};

const BYTES_PER_SAMPLE: u64 = 2;
const MAX_RECENT_TEXTS: usize = 8;
const MAX_PENDING_EVENTS: usize = 4;
const TIMESTAMP_TOLERANCE_MILLIS: u64 = 250;
const FINALIZE_SILENCE_MILLIS: u64 = 1_000;

#[derive(Debug)]
pub(super) struct LiveState {
    sample_rate_hz: u32,
    user_audio_bytes: u64,
    segment_end_millis: u64,
    committed_end_millis: u64,
    finalizing: bool,
    stable_text: String,
    recent_stable: VecDeque<String>,
    recent_committed: VecDeque<String>,
    pending: VecDeque<LiveRecognitionEvent>,
    provider_reference: Option<ProviderReference>,
}

impl LiveState {
    pub fn new(sample_rate_hz: u32, provider_reference: Option<ProviderReference>) -> Self {
        Self {
            sample_rate_hz,
            user_audio_bytes: 0,
            segment_end_millis: 0,
            committed_end_millis: 0,
            finalizing: false,
            stable_text: String::new(),
            recent_stable: VecDeque::with_capacity(MAX_RECENT_TEXTS),
            recent_committed: VecDeque::with_capacity(MAX_RECENT_TEXTS),
            pending: VecDeque::with_capacity(MAX_PENDING_EVENTS),
            provider_reference,
        }
    }

    pub fn record_audio(&mut self, bytes: usize) {
        self.user_audio_bytes = self
            .user_audio_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    pub const fn has_user_audio(&self) -> bool {
        self.user_audio_bytes != 0
    }

    pub fn begin_finalize(&mut self) {
        self.finalizing = self.has_user_audio();
    }

    pub fn cancel_finalize(&mut self) {
        self.finalizing = false;
    }

    pub fn pop_pending(&mut self) -> Option<LiveRecognitionEvent> {
        self.pending.pop_front()
    }

    pub fn provider_reference(&self) -> Option<ProviderReference> {
        self.provider_reference.clone()
    }

    pub fn apply(
        &mut self,
        message: ParsedLiveMessage,
    ) -> Result<Option<LiveRecognitionEvent>, &'static str> {
        match message {
            ParsedLiveMessage::SessionStarted(reference) => {
                if let Some(reference) = reference {
                    self.provider_reference = Some(ProviderReference::new(reference));
                }
                Ok(None)
            }
            ParsedLiveMessage::Transcript {
                kind,
                text,
                end_millis,
                confidence,
            } => self.transcript(kind, text, end_millis, confidence),
            ParsedLiveMessage::Ignore | ParsedLiveMessage::TerminalError { .. } => Ok(None),
        }
    }

    fn transcript(
        &mut self,
        kind: TranscriptKind,
        text: String,
        provider_end_millis: Option<u64>,
        confidence: Option<f32>,
    ) -> Result<Option<LiveRecognitionEvent>, &'static str> {
        let normalized = normalize(&text);
        let user_end = self.user_audio_millis();
        let allowed_drift = if self.finalizing {
            FINALIZE_SILENCE_MILLIS
        } else {
            TIMESTAMP_TOLERANCE_MILLIS
        };
        let end = match provider_end_millis {
            Some(end) if end <= user_end.saturating_add(allowed_drift) => end.min(user_end),
            Some(_) => return Err("ELEVENLABS_LIVE_INVALID_TIMING"),
            None => user_end,
        };

        match kind {
            TranscriptKind::Partial => Ok((!normalized.is_empty()).then(|| {
                transcript_event(
                    text,
                    self.current_segment_start(),
                    end,
                    confidence,
                    LiveTranscriptStability::Partial,
                )
            })),
            TranscriptKind::SegmentFinal => {
                if normalized.is_empty() || contains(&self.recent_stable, &normalized) {
                    return Ok(None);
                }
                let start = self.current_segment_start();
                self.segment_end_millis = end.max(start);
                remember(&mut self.recent_stable, normalized);
                append_stable(&mut self.stable_text, &text);
                Ok(Some(transcript_event(
                    text,
                    start,
                    self.segment_end_millis,
                    confidence,
                    LiveTranscriptStability::SegmentFinal,
                )))
            }
            TranscriptKind::UtteranceFinal => {
                let commit_start = self.committed_end_millis;
                let end = end.max(commit_start);
                let stable_boundary = self.segment_end_millis.clamp(commit_start, end);
                self.committed_end_millis = end;
                self.segment_end_millis = end;
                let reconciliation = committed_suffix_after_stable_prefix(&text, &self.stable_text);
                let had_stable = !self.stable_text.trim().is_empty();
                let duplicate = normalized.is_empty()
                    || contains(&self.recent_committed, &normalized)
                    || reconciliation == Some("")
                    || (had_stable && reconciliation.is_none());
                let (result_text, start) = match reconciliation {
                    Some(suffix) if !suffix.is_empty() => (suffix.to_owned(), stable_boundary),
                    _ => (text, commit_start),
                };
                if !normalized.is_empty() {
                    remember(&mut self.recent_committed, normalized);
                }
                self.stable_text.clear();
                let finalize_observed = std::mem::take(&mut self.finalizing);
                let event = (!duplicate).then(|| {
                    transcript_event(
                        result_text,
                        start,
                        end,
                        confidence,
                        LiveTranscriptStability::UtteranceFinal,
                    )
                });
                if finalize_observed {
                    if event.is_some() {
                        push_pending(
                            &mut self.pending,
                            LiveRecognitionEvent::FinalizeResultObserved,
                        );
                    } else {
                        return Ok(Some(LiveRecognitionEvent::FinalizeResultObserved));
                    }
                }
                Ok(event)
            }
        }
    }

    fn current_segment_start(&self) -> u64 {
        self.segment_end_millis.max(self.committed_end_millis)
    }

    fn user_audio_millis(&self) -> u64 {
        self.user_audio_bytes
            .saturating_mul(1_000)
            .checked_div(u64::from(self.sample_rate_hz) * BYTES_PER_SAMPLE)
            .unwrap_or_default()
    }
}

fn transcript_event(
    text: String,
    start_millis: u64,
    end_millis: u64,
    confidence: Option<f32>,
    stability: LiveTranscriptStability,
) -> LiveRecognitionEvent {
    LiveRecognitionEvent::Transcript(LiveTranscript {
        text,
        start_millis,
        duration_millis: end_millis.saturating_sub(start_millis),
        confidence,
        stability,
    })
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .flat_map(str::chars)
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn committed_suffix_after_stable_prefix<'a>(committed: &'a str, stable: &str) -> Option<&'a str> {
    let stable_tokens = lexical_tokens(stable);
    if stable_tokens.is_empty() {
        return None;
    }
    let committed_tokens = lexical_tokens(committed);
    if stable_tokens.len() > committed_tokens.len()
        || !stable_tokens
            .iter()
            .zip(&committed_tokens)
            .all(|((stable, _), (committed, _))| stable == committed)
    {
        return None;
    }
    if stable_tokens.len() == committed_tokens.len() {
        return Some("");
    }
    Some(committed[committed_tokens[stable_tokens.len()].1..].trim())
}

fn lexical_tokens(text: &str) -> Vec<(String, usize)> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    for (index, character) in text.char_indices() {
        if character.is_alphanumeric() {
            if current.is_empty() {
                start = index;
            }
            current.extend(character.to_lowercase());
        } else if !current.is_empty() {
            tokens.push((std::mem::take(&mut current), start));
        }
    }
    if !current.is_empty() {
        tokens.push((current, start));
    }
    tokens
}

fn contains(queue: &VecDeque<String>, normalized: &str) -> bool {
    queue.iter().any(|value| value == normalized)
}

fn remember(queue: &mut VecDeque<String>, value: String) {
    if queue.len() == MAX_RECENT_TEXTS {
        queue.pop_front();
    }
    queue.push_back(value);
}

fn append_stable(accumulator: &mut String, text: &str) {
    if !accumulator.is_empty() {
        accumulator.push(' ');
    }
    accumulator.push_str(text.trim());
}

fn push_pending(queue: &mut VecDeque<LiveRecognitionEvent>, event: LiveRecognitionEvent) {
    if queue.len() == MAX_PENDING_EVENTS {
        queue.pop_front();
    }
    queue.push_back(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: TranscriptKind, text: &str, end: Option<u64>) -> ParsedLiveMessage {
        ParsedLiveMessage::Transcript {
            kind,
            text: text.into(),
            end_millis: end,
            confidence: Some(0.9),
        }
    }

    #[test]
    fn projects_clocked_events_and_suppresses_provider_echoes() {
        let mut state = LiveState::new(48_000, None);
        state.record_audio(96_000);
        let partial = state
            .apply(message(TranscriptKind::Partial, "hel", None))
            .unwrap()
            .unwrap();
        assert!(
            matches!(partial, LiveRecognitionEvent::Transcript(ref item) if item.duration_millis == 1_000)
        );
        let stable = state
            .apply(message(TranscriptKind::SegmentFinal, "hello", Some(800)))
            .unwrap();
        assert!(stable.is_some());
        assert!(
            state
                .apply(message(TranscriptKind::SegmentFinal, "HELLO", Some(800)))
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .apply(message(TranscriptKind::UtteranceFinal, "Hello!", None))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn finalize_marker_follows_new_committed_transcript() {
        let mut state = LiveState::new(16_000, None);
        state.record_audio(32_000);
        state.begin_finalize();
        let event = state
            .apply(message(TranscriptKind::UtteranceFinal, "tail", Some(2_000)))
            .unwrap()
            .unwrap();
        assert!(matches!(event, LiveRecognitionEvent::Transcript(_)));
        assert_eq!(
            state.pop_pending(),
            Some(LiveRecognitionEvent::FinalizeResultObserved)
        );
    }

    #[test]
    fn vad_commits_create_separate_utterances_before_empty_terminal_flush() {
        let mut state = LiveState::new(16_000, None);
        state.record_audio(32_000);
        let first = state
            .apply(message(TranscriptKind::UtteranceFinal, "question", None))
            .unwrap()
            .unwrap();
        state.record_audio(64_000);
        let second = state
            .apply(message(
                TranscriptKind::UtteranceFinal,
                "group farewell",
                None,
            ))
            .unwrap()
            .unwrap();
        let (LiveRecognitionEvent::Transcript(first), LiveRecognitionEvent::Transcript(second)) =
            (first, second)
        else {
            panic!("expected separate VAD utterances");
        };
        assert_eq!((first.start_millis, first.duration_millis), (0, 1_000));
        assert_eq!(
            (second.start_millis, second.duration_millis),
            (1_000, 2_000)
        );

        state.begin_finalize();
        assert_eq!(
            state
                .apply(message(TranscriptKind::UtteranceFinal, "", None))
                .unwrap(),
            Some(LiveRecognitionEvent::FinalizeResultObserved)
        );
    }
}
