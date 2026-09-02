//! Consumer-owned asynchronous ports and provider-neutral application models.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;

use super::batch_capabilities::BatchCapabilityDescriptor;
use super::live_capabilities::LiveCapabilityDescriptor;
use crate::domain::batch::{BatchJob, BatchProfile};
use crate::domain::live::RawAudioSequence;

/// Sendable heap-allocated future used to keep asynchronous ports object-safe.
pub type BoxFuture<'a, Output> = Pin<Box<dyn Future<Output = Output> + Send + 'a>>;

/// Opaque durable batch identity selected outside persistence adapters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BatchJobId(Box<str>);

impl BatchJobId {
    /// Wraps an application-owned job identity.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().into_boxed_str())
    }

    /// Returns the opaque identity without assigning transport semantics.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Opaque provider request reference safe to persist and expose diagnostically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderReference(Box<str>);

impl ProviderReference {
    /// Wraps an adapter-normalized provider reference.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().into_boxed_str())
    }

    /// Returns the provider-neutral reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact provider-egress classification used by batch and live adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecognitionFailureClass {
    /// Provider acceptance is known to be false; retry is an explicit property.
    KnownNotAccepted { retryable: bool },
    /// Provider acceptance is known and the provider reported a terminal failure.
    KnownAcceptedTerminal,
    /// Egress may have reached the provider, so another paid submission is unsafe.
    UnknownAfterSend,
}

/// Provider-neutral failure evidence returned by recognition adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecognitionFailure {
    /// Provider is known not to have accepted this submission.
    KnownNotAccepted {
        retryable: bool,
        code: String,
        provider_reference: Option<ProviderReference>,
        retry_after_millis: Option<u64>,
    },
    /// Provider accepted the submission and returned an explicit terminal failure.
    KnownAcceptedTerminal {
        code: String,
        provider_reference: Option<ProviderReference>,
    },
    /// Submission outcome cannot be proven after provider egress began.
    UnknownAfterSend {
        code: String,
        provider_reference: Option<ProviderReference>,
    },
}

impl RecognitionFailure {
    /// Returns the retry-safety classification without inspecting adapter codes.
    pub const fn class(&self) -> RecognitionFailureClass {
        match self {
            Self::KnownNotAccepted { retryable, .. } => RecognitionFailureClass::KnownNotAccepted {
                retryable: *retryable,
            },
            Self::KnownAcceptedTerminal { .. } => RecognitionFailureClass::KnownAcceptedTerminal,
            Self::UnknownAfterSend { .. } => RecognitionFailureClass::UnknownAfterSend,
        }
    }

    /// Returns the stable application failure code.
    pub fn code(&self) -> &str {
        match self {
            Self::KnownNotAccepted { code, .. }
            | Self::KnownAcceptedTerminal { code, .. }
            | Self::UnknownAfterSend { code, .. } => code,
        }
    }

    /// Returns an optional bounded provider request reference.
    pub const fn provider_reference(&self) -> Option<&ProviderReference> {
        match self {
            Self::KnownNotAccepted {
                provider_reference, ..
            }
            | Self::KnownAcceptedTerminal {
                provider_reference, ..
            }
            | Self::UnknownAfterSend {
                provider_reference, ..
            } => provider_reference.as_ref(),
        }
    }
}

/// Provider-neutral batch recognition input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchRecognitionRequest {
    pub profile: BatchProfile,
    pub audio: Vec<u8>,
    pub authoritative_duration_millis: u64,
    pub keyterms: Vec<String>,
}

/// One authoritative provider-neutral batch transcript segment.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchSegment {
    pub start_millis: u64,
    pub end_millis: u64,
    pub text: String,
    pub confidence: Option<f32>,
    pub speaker: Option<String>,
}

/// Optional readable projection linked back to authoritative raw segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReadableSegment {
    pub start_millis: u64,
    pub end_millis: u64,
    pub text: String,
    pub source_segment_indices: Vec<usize>,
}

/// Bounded provider-neutral batch recognition output.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchRecognitionResult {
    pub profile: BatchProfile,
    pub text: String,
    pub duration_millis: u64,
    pub provider_duration_millis: Option<u64>,
    pub segments: Vec<BatchSegment>,
    pub readable_segments: Option<Vec<BatchReadableSegment>>,
    pub provider_reference: Option<ProviderReference>,
}

/// A completed provider result cannot be represented by the consumer-owned outbound contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchResultProjectionFailure;

/// Consumer-owned check performed before a successful provider result becomes durable completion.
pub trait BatchResultProjection: Send + Sync {
    /// Validates the exact externally served projection without exposing serialization here.
    fn validate(
        &self,
        id: &BatchJobId,
        result: &BatchRecognitionResult,
    ) -> Result<(), BatchResultProjectionFailure>;
}

/// Paid pre-recorded recognition capability.
pub trait BatchRecognizer: Send + Sync {
    /// Returns the immutable identity and limitations implemented by this adapter.
    fn capabilities(&self) -> &'static BatchCapabilityDescriptor;

    /// Submits exactly one bound request. The caller owns retry decisions from the returned class.
    fn recognize(
        &self,
        request: BatchRecognitionRequest,
    ) -> BoxFuture<'_, Result<BatchRecognitionResult, RecognitionFailure>>;
}

/// Opaque durable spool reference; it deliberately contains no filesystem path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchAudioHandle(Box<str>);

impl BatchAudioHandle {
    /// Wraps an adapter-owned spool reference.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().into_boxed_str())
    }

    /// Returns the opaque spool reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Durable job snapshot returned by a [`BatchJobStore`].
#[derive(Clone, Debug, PartialEq)]
pub struct BatchJobSnapshot {
    pub id: BatchJobId,
    pub job: BatchJob,
    pub audio: BatchAudioHandle,
    pub authoritative_duration_millis: u64,
    pub keyterms: Vec<String>,
    pub provider_reference: Option<ProviderReference>,
    pub retry_after_millis: Option<u64>,
    pub result: Option<BatchRecognitionResult>,
    pub revision: u64,
}

/// Atomic insert outcome for an idempotent job identity.
#[derive(Clone, Debug, PartialEq)]
pub enum BatchJobInsertOutcome {
    Inserted(BatchJobSnapshot),
    Existing(BatchJobSnapshot),
}

/// Compare-and-swap outcome for durable lifecycle updates.
#[derive(Clone, Debug, PartialEq)]
pub enum BatchJobUpdateOutcome {
    Stored(BatchJobSnapshot),
    RevisionConflict(BatchJobSnapshot),
    Missing,
}

/// Failure to access or decode durable batch job state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchJobStoreFailure {
    Unavailable { code: String },
    InvalidSnapshot { code: String },
}

/// Linearizable durable job ledger capability with optimistic concurrency.
pub trait BatchJobStore: Send + Sync {
    /// Loads one job identity, returning `None` when it has never been accepted.
    fn load<'a>(
        &'a self,
        id: &'a BatchJobId,
    ) -> BoxFuture<'a, Result<Option<BatchJobSnapshot>, BatchJobStoreFailure>>;

    /// Atomically inserts the initial accepted job or returns the existing identity.
    fn insert(
        &self,
        id: BatchJobId,
        job: BatchJob,
        audio: BatchAudioHandle,
        authoritative_duration_millis: u64,
        keyterms: Vec<String>,
    ) -> BoxFuture<'_, Result<BatchJobInsertOutcome, BatchJobStoreFailure>>;

    /// Stores a replacement only when `expected_revision` still owns the current snapshot.
    fn compare_and_swap(
        &self,
        expected_revision: u64,
        replacement: BatchJobSnapshot,
    ) -> BoxFuture<'_, Result<BatchJobUpdateOutcome, BatchJobStoreFailure>>;

    /// Returns the greatest currently recoverable identity, freezing one startup scan.
    fn recovery_head(&self) -> BoxFuture<'_, Result<Option<BatchJobId>, BatchJobStoreFailure>> {
        Box::pin(async {
            Err(BatchJobStoreFailure::Unavailable {
                code: "RECOVERY_HEAD_UNSUPPORTED".into(),
            })
        })
    }

    /// Lists an identity-ordered bounded page after the exclusive cursor.
    fn list_recovery_candidates(
        &self,
        after: Option<BatchJobId>,
        maximum: NonZeroUsize,
    ) -> BoxFuture<'_, Result<Vec<BatchJobSnapshot>, BatchJobStoreFailure>>;

    /// Lists a bounded page through an inclusive frozen head.
    fn list_recovery_candidates_through(
        &self,
        after: Option<BatchJobId>,
        through: BatchJobId,
        maximum: NonZeroUsize,
    ) -> BoxFuture<'_, Result<Vec<BatchJobSnapshot>, BatchJobStoreFailure>> {
        Box::pin(async move {
            let mut candidates = self.list_recovery_candidates(after, maximum).await?;
            candidates.retain(|snapshot| snapshot.id.as_str() <= through.as_str());
            Ok(candidates)
        })
    }
}

/// Failure to access the durable bounded audio spool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchAudioSpoolFailure {
    Unavailable { code: String },
    Missing,
    IdentityConflict,
    CapacityExceeded,
}

/// Atomic spool outcome used to keep replay cleanup ownership explicit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchAudioStoreOutcome {
    /// This call created the artifact and therefore owns orphan cleanup.
    Stored(BatchAudioHandle),
    /// An exact artifact already existed and must not be removed by this caller.
    Existing(BatchAudioHandle),
}

/// Durable atomic batch-audio storage capability.
pub trait BatchAudioSpool: Send + Sync {
    /// Atomically stores bytes for one job and reports whether this call owns the artifact.
    fn store(
        &self,
        id: BatchJobId,
        audio: Vec<u8>,
    ) -> BoxFuture<'_, Result<BatchAudioStoreOutcome, BatchAudioSpoolFailure>>;

    /// Reads the complete accepted artifact addressed by an opaque handle.
    fn read<'a>(
        &'a self,
        handle: &'a BatchAudioHandle,
    ) -> BoxFuture<'a, Result<Vec<u8>, BatchAudioSpoolFailure>>;

    /// Removes an orphan or terminal artifact. Implementations must be idempotent.
    fn remove<'a>(
        &'a self,
        handle: &'a BatchAudioHandle,
    ) -> BoxFuture<'a, Result<(), BatchAudioSpoolFailure>>;
}

/// Immutable live provider/model identity selected before opening a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveProfile {
    pub protocol_version: u16,
    pub provider: String,
    pub model: String,
    pub language: String,
}

/// Provider-neutral configuration for one live recognizer session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveRecognitionRequest {
    pub profile: LiveProfile,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub keyterms: Vec<String>,
}

/// One normalized PCM S16LE write correlated to the gateway's raw-audio sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveAudioFrame {
    pub sequence: RawAudioSequence,
    pub pcm_s16le: Vec<u8>,
}

/// Stability of one normalized provider transcript event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveTranscriptStability {
    Partial,
    SegmentFinal,
    UtteranceFinal,
}

/// Provider-neutral live transcript timing and text.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveTranscript {
    pub text: String,
    pub start_millis: u64,
    pub duration_millis: u64,
    pub confidence: Option<f32>,
    pub stability: LiveTranscriptStability,
}

/// Normalized event stream emitted by a live recognizer.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveRecognitionEvent {
    Transcript(LiveTranscript),
    UtteranceEnd { last_word_end_millis: u64 },
    FinalizeResultObserved,
}

/// Creates provider-bound live sessions without coupling callers to an adapter.
pub trait LiveRecognizerFactory: Send + Sync {
    /// Returns the immutable identity and limitations implemented by this adapter.
    fn capabilities(&self) -> &'static LiveCapabilityDescriptor;

    /// Opens exactly the requested configured profile; implementations never fall back.
    fn open(
        &self,
        request: LiveRecognitionRequest,
    ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>>;
}

/// One open provider-bound live recognition session.
pub trait LiveRecognizerSession: Send + Sync {
    /// Completes only after the bounded provider write succeeds.
    fn write_audio(&self, frame: LiveAudioFrame) -> BoxFuture<'_, Result<(), RecognitionFailure>>;

    /// Receives the next normalized event, or `None` after provider stream closure.
    fn next_event(&self)
    -> BoxFuture<'_, Result<Option<LiveRecognitionEvent>, RecognitionFailure>>;

    /// Initiates provider-specific finalization without claiming that a result was observed.
    fn finalize(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>>;

    /// Closes provider resources after bounded finalization or failure handling.
    fn close(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::batch::BatchRequestFingerprint;
    use crate::domain::live::LiveSession;

    #[test]
    fn every_async_port_is_object_safe() {
        let _: Option<&dyn BatchRecognizer> = None;
        let _: Option<&dyn BatchJobStore> = None;
        let _: Option<&dyn BatchAudioSpool> = None;
        let _: Option<&dyn LiveRecognizerFactory> = None;
        let _: Option<&dyn LiveRecognizerSession> = None;
    }

    #[test]
    fn failure_classes_preserve_retry_safety() {
        let retryable = RecognitionFailure::KnownNotAccepted {
            retryable: true,
            code: "CAPACITY_BUSY".into(),
            provider_reference: None,
            retry_after_millis: Some(250),
        };
        let terminal = RecognitionFailure::KnownAcceptedTerminal {
            code: "TERMINAL".into(),
            provider_reference: Some(ProviderReference::new("request-1")),
        };
        let unknown = RecognitionFailure::UnknownAfterSend {
            code: "UNCERTAIN".into(),
            provider_reference: None,
        };

        assert_eq!(
            retryable.class(),
            RecognitionFailureClass::KnownNotAccepted { retryable: true }
        );
        assert_eq!(
            terminal.class(),
            RecognitionFailureClass::KnownAcceptedTerminal
        );
        assert_eq!(unknown.class(), RecognitionFailureClass::UnknownAfterSend);
        assert_eq!(terminal.provider_reference().unwrap().as_str(), "request-1");
    }

    #[test]
    fn snapshots_reuse_domain_job_identity() {
        let profile = BatchProfile::new(2, "provider-a", "model-a", "multi").unwrap();
        let fingerprint = BatchRequestFingerprint::from_bytes([9; 32]);
        let job = BatchJob::accept(profile.clone(), fingerprint);
        let snapshot = BatchJobSnapshot {
            id: BatchJobId::new("job-1"),
            job,
            audio: BatchAudioHandle::new("audio-1"),
            authoritative_duration_millis: 1_000,
            keyterms: vec!["alpha".into()],
            provider_reference: None,
            retry_after_millis: None,
            result: None,
            revision: 0,
        };

        assert_eq!(snapshot.job.profile(), &profile);
        assert_eq!(snapshot.job.fingerprint(), fingerprint);
        assert_eq!(snapshot.id.as_str(), "job-1");
        assert_eq!(snapshot.audio.as_str(), "audio-1");
    }

    #[test]
    fn live_frames_reuse_domain_audio_sequences() {
        let mut session = LiveSession::new();
        session.mark_ready().unwrap();
        let sequence = session.accept_audio().unwrap();
        let frame = LiveAudioFrame {
            sequence,
            pcm_s16le: vec![0, 0],
        };

        assert_eq!(frame.sequence.get(), 1);
        assert_eq!(frame.pcm_s16le, vec![0, 0]);
    }
}
