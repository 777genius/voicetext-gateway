//! Opt-in, bounded qualification observations kept outside public `VoiceText` contracts.

use std::future::Future;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use voicetext_speech::application::ports::{
    BatchRecognitionResult, LiveRecognitionEvent, ProviderOperation, ProviderOperationKind,
};

const FRAME_DIGEST_PREFIX: &[u8] = b"voicetext-acked-frames-v1\n";
const RESULT_DIGEST_PREFIX: &[u8] = b"voicetext-normalized-result-v1\n";
pub(crate) const OBSERVATION_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

pub type ObservationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ObservationSinkFailure>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationSinkFailure(pub &'static str);

pub trait BatchObservationSink: Send + Sync {
    fn enabled(&self) -> bool;
    fn observe_batch(&self, record: BatchObservation) -> ObservationFuture<'_>;
}

pub trait LiveObservationSink: Send + Sync {
    fn enabled(&self) -> bool;
    fn observe_live(&self, record: LiveObservation) -> ObservationFuture<'_>;
}

#[derive(Debug, Default)]
pub struct NoBatchObservationSink;

impl BatchObservationSink for NoBatchObservationSink {
    fn enabled(&self) -> bool {
        false
    }

    fn observe_batch(&self, _record: BatchObservation) -> ObservationFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug, Default)]
pub struct NoLiveObservationSink;

impl LiveObservationSink for NoLiveObservationSink {
    fn enabled(&self) -> bool {
        false
    }

    fn observe_live(&self, _record: LiveObservation) -> ObservationFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ObservationProfile {
    pub contract_version: u16,
    pub provider: String,
    pub model: String,
    pub language: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct OperationObservation {
    pub kind: &'static str,
    pub id: String,
}

impl From<ProviderOperation> for OperationObservation {
    fn from(value: ProviderOperation) -> Self {
        let kind = match value.kind() {
            ProviderOperationKind::RequestId => "request_id",
            ProviderOperationKind::TranscriptionId => "transcription_id",
            ProviderOperationKind::SessionId => "session_id",
        };
        Self {
            kind,
            id: value.id().to_owned(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BatchObservation {
    pub schema: &'static str,
    pub effect_id: Uuid,
    pub gateway_job_id: String,
    pub profile: ObservationProfile,
    pub provider_operation: Option<OperationObservation>,
    pub result_digest: Option<String>,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: u64,
    pub terminal_status: &'static str,
    pub durable_persistence: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct SequenceObservation {
    pub count: u64,
    pub first: Option<u64>,
    pub last: Option<u64>,
    pub contiguous: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveObservation {
    pub schema: &'static str,
    pub effect_id: Uuid,
    pub client_session_id: Uuid,
    pub gateway_session_id: Uuid,
    pub profile: ObservationProfile,
    pub provider_operation: Option<OperationObservation>,
    pub accepted_frame_count: u64,
    pub written_sequences: SequenceObservation,
    pub acked_sequences: SequenceObservation,
    pub acked_raw_input_digest: String,
    pub result_digest: String,
    pub finalize_result_observed: bool,
    pub terminal_status: String,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: u64,
}

mod file_sink;
pub use file_sink::FileQualificationSink;

#[derive(Debug)]
pub(crate) struct LiveObservationTracker {
    effect_id: Uuid,
    client_session_id: Uuid,
    gateway_session_id: Uuid,
    profile: ObservationProfile,
    started_at_unix_ms: u64,
    accepted_frame_count: u64,
    written: SequenceTracker,
    acked: SequenceTracker,
    frame_digest: Sha256,
    result_digest: Sha256,
    finalize_result_observed: bool,
}

impl LiveObservationTracker {
    pub(crate) fn new(
        client_session_id: Uuid,
        gateway_session_id: Uuid,
        profile: ObservationProfile,
    ) -> Self {
        let mut frame_digest = Sha256::new();
        frame_digest.update(FRAME_DIGEST_PREFIX);
        let mut result_digest = Sha256::new();
        result_digest.update(RESULT_DIGEST_PREFIX);
        Self {
            effect_id: Uuid::new_v4(),
            client_session_id,
            gateway_session_id,
            profile,
            started_at_unix_ms: unix_millis(),
            accepted_frame_count: 0,
            written: SequenceTracker::default(),
            acked: SequenceTracker::default(),
            frame_digest,
            result_digest,
            finalize_result_observed: false,
        }
    }

    pub(crate) fn accept_frame(&mut self) {
        self.accepted_frame_count = self.accepted_frame_count.saturating_add(1);
    }

    pub(crate) fn provider_written(&mut self, sequence: u64) {
        self.written.push(sequence);
    }

    pub(crate) fn ack_sent(&mut self, sequence: u64, raw: &[u8]) {
        self.acked.push(sequence);
        self.frame_digest.update(sequence.to_be_bytes());
        self.frame_digest
            .update(u64::try_from(raw.len()).unwrap_or(u64::MAX).to_be_bytes());
        self.frame_digest.update(raw);
    }

    pub(crate) fn provider_event(&mut self, event: &LiveRecognitionEvent) {
        match event {
            LiveRecognitionEvent::Transcript(value) => {
                self.result_digest.update(b"transcript\0");
                digest_bytes(&mut self.result_digest, value.text.as_bytes());
                self.result_digest.update(value.start_millis.to_be_bytes());
                self.result_digest
                    .update(value.duration_millis.to_be_bytes());
                self.result_digest.update(
                    value
                        .confidence
                        .map_or(u32::MAX, f32::to_bits)
                        .to_be_bytes(),
                );
                self.result_digest.update([value.stability as u8]);
            }
            LiveRecognitionEvent::UtteranceEnd {
                last_word_end_millis,
            } => {
                self.result_digest.update(b"utterance_end\0");
                self.result_digest
                    .update(last_word_end_millis.to_be_bytes());
            }
            LiveRecognitionEvent::FinalizeResultObserved => {
                self.result_digest.update(b"finalize_result\0");
                self.finalize_result_observed = true;
            }
        }
    }

    pub(crate) fn finish(
        self,
        provider_operation: Option<ProviderOperation>,
        terminal_status: String,
    ) -> LiveObservation {
        LiveObservation {
            schema: "voicetext-qualification-observation-v1",
            effect_id: self.effect_id,
            client_session_id: self.client_session_id,
            gateway_session_id: self.gateway_session_id,
            profile: self.profile,
            provider_operation: provider_operation.map(Into::into),
            accepted_frame_count: self.accepted_frame_count,
            written_sequences: self.written.finish(),
            acked_sequences: self.acked.finish(),
            acked_raw_input_digest: hex::encode(self.frame_digest.finalize()),
            result_digest: hex::encode(self.result_digest.finalize()),
            finalize_result_observed: self.finalize_result_observed,
            terminal_status,
            started_at_unix_ms: self.started_at_unix_ms,
            finished_at_unix_ms: unix_millis(),
        }
    }
}

#[derive(Debug, Default)]
struct SequenceTracker {
    count: u64,
    first: Option<u64>,
    last: Option<u64>,
    contiguous: bool,
}

impl SequenceTracker {
    fn push(&mut self, value: u64) {
        self.contiguous = self.count == 0
            || (self.contiguous
                && self
                    .last
                    .is_some_and(|last| last.checked_add(1) == Some(value)));
        self.first.get_or_insert(value);
        self.last = Some(value);
        self.count = self.count.saturating_add(1);
    }

    fn finish(self) -> SequenceObservation {
        SequenceObservation {
            count: self.count,
            first: self.first,
            last: self.last,
            contiguous: self.count == 0 || self.contiguous,
        }
    }
}

pub(crate) fn batch_result_digest(result: &BatchRecognitionResult) -> String {
    let mut digest = Sha256::new();
    digest.update(RESULT_DIGEST_PREFIX);
    for value in [
        result.profile.provider(),
        result.profile.model(),
        result.profile.language(),
        &result.text,
    ] {
        digest_bytes(&mut digest, value.as_bytes());
    }
    digest.update(result.profile.contract_version().to_be_bytes());
    digest.update(result.duration_millis.to_be_bytes());
    digest.update(
        result
            .provider_duration_millis
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(
        u64::try_from(result.segments.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for segment in &result.segments {
        digest.update(segment.start_millis.to_be_bytes());
        digest.update(segment.end_millis.to_be_bytes());
        digest_bytes(&mut digest, segment.text.as_bytes());
        digest.update(
            segment
                .confidence
                .map_or(u32::MAX, f32::to_bits)
                .to_be_bytes(),
        );
        digest_bytes(
            &mut digest,
            segment.speaker.as_deref().unwrap_or("").as_bytes(),
        );
    }
    hex::encode(digest.finalize())
}

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn digest_bytes(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

pub(super) fn valid_campaign(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests;
