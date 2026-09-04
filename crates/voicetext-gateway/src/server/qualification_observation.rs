//! Opt-in, bounded qualification observations kept outside public VoiceText contracts.

use std::fmt;
use std::future::Future;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use voicetext_speech::application::ports::{
    BatchRecognitionResult, LiveRecognitionEvent, ProviderOperation, ProviderOperationKind,
};

const MAX_OBSERVATIONS: usize = 64;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const FRAME_DIGEST_PREFIX: &[u8] = b"voicetext-acked-frames-v1\n";
const RESULT_DIGEST_PREFIX: &[u8] = b"voicetext-normalized-result-v1\n";

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

pub struct FileQualificationSink {
    directory: PathBuf,
    campaign: Box<str>,
    written: AtomicUsize,
}

impl fmt::Debug for FileQualificationSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileQualificationSink")
            .field("directory", &self.directory)
            .field("campaign", &self.campaign)
            .finish_non_exhaustive()
    }
}

impl FileQualificationSink {
    pub fn new(directory: &Path, campaign: &str) -> Result<Self, ObservationSinkFailure> {
        if !valid_campaign(campaign) || !directory.is_absolute() {
            return Err(ObservationSinkFailure(
                "INVALID_QUALIFICATION_CONFIGURATION",
            ));
        }
        let metadata = std::fs::symlink_metadata(directory)
            .map_err(|_| ObservationSinkFailure("QUALIFICATION_DIRECTORY_UNAVAILABLE"))?;
        let self_uid = std::fs::metadata("/proc/self")
            .map_err(|_| ObservationSinkFailure("QUALIFICATION_CUSTODY_UNAVAILABLE"))?
            .uid();
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != self_uid
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(ObservationSinkFailure("QUALIFICATION_DIRECTORY_UNSAFE"));
        }
        Ok(Self {
            directory: directory.to_owned(),
            campaign: campaign.into(),
            written: AtomicUsize::new(0),
        })
    }

    fn write<T: Serialize + Send + 'static>(
        &self,
        mode: &'static str,
        effect_id: Uuid,
        record: T,
    ) -> ObservationFuture<'_> {
        let slot = self.written.fetch_add(1, Ordering::AcqRel);
        if slot >= MAX_OBSERVATIONS {
            self.written.fetch_sub(1, Ordering::AcqRel);
            return Box::pin(async { Err(ObservationSinkFailure("QUALIFICATION_RECORD_LIMIT")) });
        }
        let path = self
            .directory
            .join(format!("{}-{mode}-{effect_id}.json", self.campaign));
        Box::pin(async move {
            let bytes = serde_json::to_vec(&record)
                .map_err(|_| ObservationSinkFailure("QUALIFICATION_SERIALIZE_FAILED"))?;
            if bytes.len() > MAX_RECORD_BYTES {
                return Err(ObservationSinkFailure("QUALIFICATION_RECORD_TOO_LARGE"));
            }
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true).mode(0o600);
            let mut file = options
                .open(path)
                .await
                .map_err(|_| ObservationSinkFailure("QUALIFICATION_CREATE_FAILED"))?;
            file.write_all(&bytes)
                .await
                .map_err(|_| ObservationSinkFailure("QUALIFICATION_WRITE_FAILED"))?;
            file.sync_all()
                .await
                .map_err(|_| ObservationSinkFailure("QUALIFICATION_SYNC_FAILED"))?;
            Ok(())
        })
    }
}

impl BatchObservationSink for FileQualificationSink {
    fn enabled(&self) -> bool {
        true
    }

    fn observe_batch(&self, record: BatchObservation) -> ObservationFuture<'_> {
        self.write("batch", record.effect_id, record)
    }
}

impl LiveObservationSink for FileQualificationSink {
    fn enabled(&self) -> bool {
        true
    }

    fn observe_live(&self, record: LiveObservation) -> ObservationFuture<'_> {
        self.write("live", record.effect_id, record)
    }
}

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
                        .map(f32::to_bits)
                        .unwrap_or(u32::MAX)
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
        self.contiguous = self.count == 0 || self.last.is_some_and(|last| value == last + 1);
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
                .map(f32::to_bits)
                .unwrap_or(u32::MAX)
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

fn valid_campaign(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests;
