//! Coordinator-owned batch request, outcome, and validation models.

use crate::application::ports::{
    BatchAudioSpoolFailure, BatchJobId, BatchJobSnapshot, BatchJobStoreFailure,
    BatchRecognitionResult, BatchResultProjection, RecognitionFailure,
};
use crate::domain::batch::{
    BatchFailure, BatchProfile, BatchRequestFingerprint, BatchTransitionError,
};

const MAX_SEGMENTS: usize = 10_000;
// Provider-neutral in-memory text budget across the transcript, segments, speakers, and readable
// projection. Persistence adapters independently choose and validate their representation.
const MAX_TRANSCRIPT_BYTES: usize = 4 * 1_024 * 1_024;
const MAX_READABLE_REFERENCES: usize = 100_000;

/// Complete immutable input accepted before any provider egress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchAdmissionRequest {
    pub id: BatchJobId,
    pub profile: BatchProfile,
    pub fingerprint: BatchRequestFingerprint,
    pub audio: Vec<u8>,
    pub authoritative_duration_millis: u64,
    pub keyterms: Vec<String>,
}

/// Successful idempotent admission classification.
#[derive(Clone, Debug, PartialEq)]
pub enum BatchAdmissionOutcome {
    /// This call durably accepted the job.
    Accepted(BatchJobSnapshot),
    /// The same fingerprint and profile were already accepted.
    Replay(BatchJobSnapshot),
}

/// Result of trying to claim and execute one actionable job.
#[derive(Clone, Debug, PartialEq)]
pub enum BatchExecutionOutcome {
    /// The provider outcome was durably recorded.
    Persisted(BatchJobSnapshot),
    /// Another caller won or invalidated the submission fence.
    NotClaimed(Option<BatchJobSnapshot>),
    /// The loaded job was not `Accepted` or `Retryable`.
    NotActionable(BatchJobSnapshot),
}

/// Deterministic startup reconciliation report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BatchStartupRecovery {
    pub actionable: Vec<BatchJobSnapshot>,
    pub recovered_unknown: Vec<BatchJobSnapshot>,
    pub conflicts: Vec<BatchJobSnapshot>,
    pub missing: Vec<BatchJobId>,
    pub invalid_candidates: Vec<BatchJobSnapshot>,
    pub next_cursor: Option<BatchJobId>,
}

/// Failure that preserves whether provider egress may already have happened.
#[derive(Clone, Debug, PartialEq)]
pub enum BatchCoordinatorFailure {
    /// The job identity was already bound to another fingerprint or profile.
    AdmissionConflict(Box<BatchJobSnapshot>),
    /// Durable spool access failed before provider egress.
    Spool(BatchAudioSpoolFailure),
    /// Ledger access failed before provider egress.
    Store(BatchJobStoreFailure),
    /// Admission could not prove whether the insert committed.
    AdmissionStoreUncertain {
        insert: BatchJobStoreFailure,
        verification: BatchJobStoreFailure,
    },
    /// Rollback of an artifact created by this admission failed.
    AdmissionCleanup {
        store: Option<BatchJobStoreFailure>,
        spool: BatchAudioSpoolFailure,
    },
    /// The durable domain state rejected the requested transition.
    Transition(BatchTransitionError),
    /// Ledger persistence failed after recognition was invoked; never retry automatically.
    PostEgressStore(BatchJobStoreFailure),
    /// The submission fence was lost after recognition was invoked; never retry automatically.
    PostEgressConflict,
}

pub(crate) fn apply_recognition_outcome(
    mut snapshot: BatchJobSnapshot,
    recognition: Result<BatchRecognitionResult, RecognitionFailure>,
    projection: &dyn BatchResultProjection,
) -> Result<BatchJobSnapshot, BatchTransitionError> {
    snapshot.retry_after_millis = None;
    match recognition {
        Ok(result) => {
            let valid = valid_result(&snapshot, &result);
            let projectable = valid && projection.validate(&snapshot.id, &result).is_ok();
            snapshot
                .provider_reference
                .clone_from(&result.provider_reference);
            if projectable {
                snapshot.result = Some(result);
                snapshot.job.complete()
            } else {
                snapshot.result = None;
                let code = if valid {
                    "PROVIDER_RESULT_PROJECTION_FAILED"
                } else {
                    "INVALID_RECOGNITION_RESULT"
                };
                snapshot.job.fail(batch_failure(code))
            }
        }
        Err(RecognitionFailure::KnownNotAccepted {
            retryable,
            code,
            provider_reference,
            retry_after_millis,
        }) => {
            snapshot.provider_reference = provider_reference;
            let evidence = batch_failure(&code);
            if retryable {
                snapshot.retry_after_millis = retry_after_millis;
                snapshot.job.record_retryable_failure(evidence)
            } else {
                snapshot.job.fail(evidence)
            }
        }
        Err(RecognitionFailure::KnownAcceptedTerminal {
            code,
            provider_reference,
        }) => {
            snapshot.provider_reference = provider_reference;
            snapshot.job.fail(batch_failure(&code))
        }
        Err(RecognitionFailure::UnknownAfterSend {
            code,
            provider_reference,
        }) => {
            snapshot.provider_reference = provider_reference;
            snapshot.job.record_unknown_outcome(batch_failure(&code))
        }
    }?;
    Ok(snapshot)
}

fn batch_failure(code: &str) -> BatchFailure {
    BatchFailure::new(code)
        .unwrap_or_else(|_| BatchFailure::new("INVALID_RECOGNITION_FAILURE").unwrap())
}

fn valid_result(snapshot: &BatchJobSnapshot, result: &BatchRecognitionResult) -> bool {
    if result.profile != *snapshot.job.profile()
        || result.duration_millis != snapshot.authoritative_duration_millis
        || result.text.len() > MAX_TRANSCRIPT_BYTES
        || result.segments.len() > MAX_SEGMENTS
    {
        return false;
    }
    let mut previous_end = 0;
    let mut text_bytes = result.text.len();
    for segment in &result.segments {
        let Some(next_text_bytes) = text_bytes.checked_add(segment.text.len()) else {
            return false;
        };
        let Some(next_text_bytes) = segment
            .speaker
            .as_ref()
            .map_or(Some(next_text_bytes), |speaker| {
                next_text_bytes.checked_add(speaker.len())
            })
        else {
            return false;
        };
        if segment.start_millis < previous_end
            || segment.end_millis <= segment.start_millis
            || segment.end_millis > result.duration_millis
            || segment.text.is_empty()
            || next_text_bytes > MAX_TRANSCRIPT_BYTES
            || segment
                .confidence
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return false;
        }
        text_bytes = next_text_bytes;
        previous_end = segment.end_millis;
    }
    result.readable_segments.as_ref().is_none_or(|segments| {
        if segments.len() > MAX_SEGMENTS {
            return false;
        }
        let mut previous_end = 0;
        let mut references = 0_usize;
        let mut total_text_bytes = text_bytes;
        segments.iter().all(|segment| {
            let Some(next_references) =
                references.checked_add(segment.source_segment_indices.len())
            else {
                return false;
            };
            let Some(next_text_bytes) = total_text_bytes.checked_add(segment.text.len()) else {
                return false;
            };
            let sources_valid = !segment.source_segment_indices.is_empty()
                && segment
                    .source_segment_indices
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
                && segment
                    .source_segment_indices
                    .last()
                    .is_some_and(|index| *index < result.segments.len());
            let valid = segment.start_millis >= previous_end
                && segment.end_millis > segment.start_millis
                && segment.end_millis <= result.duration_millis
                && !segment.text.is_empty()
                && next_references <= MAX_READABLE_REFERENCES
                && next_text_bytes <= MAX_TRANSCRIPT_BYTES
                && sources_valid;
            references = next_references;
            total_text_bytes = next_text_bytes;
            previous_end = segment.end_millis;
            valid
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{BatchAudioHandle, BatchResultProjectionFailure, BatchSegment};
    use crate::domain::batch::{BatchJob, BatchJobStatus};

    fn profile() -> BatchProfile {
        BatchProfile::new(2, "provider", "model", "multi").unwrap()
    }

    fn result() -> BatchRecognitionResult {
        BatchRecognitionResult {
            profile: profile(),
            text: "hello".into(),
            duration_millis: 100,
            provider_duration_millis: Some(99),
            segments: vec![BatchSegment {
                start_millis: 0,
                end_millis: 100,
                text: "hello".into(),
                confidence: Some(0.9),
                speaker: None,
            }],
            readable_segments: None,
            provider_reference: None,
        }
    }

    fn outcome_state(
        recognition: Result<BatchRecognitionResult, RecognitionFailure>,
    ) -> (BatchJobStatus, Option<u64>) {
        let mut snapshot = BatchJobSnapshot {
            id: BatchJobId::new("job"),
            job: BatchJob::accept(profile(), BatchRequestFingerprint::from_bytes([1; 32])),
            audio: BatchAudioHandle::new("audio"),
            authoritative_duration_millis: 100,
            keyterms: Vec::new(),
            provider_reference: None,
            retry_after_millis: Some(999),
            result: None,
            revision: 1,
        };
        snapshot.job.begin_submission().unwrap();
        struct AcceptProjection;
        impl BatchResultProjection for AcceptProjection {
            fn validate(
                &self,
                _id: &BatchJobId,
                _result: &BatchRecognitionResult,
            ) -> Result<(), BatchResultProjectionFailure> {
                Ok(())
            }
        }
        let outcome = apply_recognition_outcome(snapshot, recognition, &AcceptProjection).unwrap();
        (outcome.job.state().status(), outcome.retry_after_millis)
    }

    #[test]
    fn failure_mapping_persists_only_retryable_delay() {
        let known = |retryable, code: &str| RecognitionFailure::KnownNotAccepted {
            retryable,
            code: code.into(),
            provider_reference: None,
            retry_after_millis: retryable.then_some(250),
        };
        let cases = [
            (known(true, "RETRY"), BatchJobStatus::Retryable),
            (known(false, "REJECTED"), BatchJobStatus::Failed),
            (
                RecognitionFailure::KnownAcceptedTerminal {
                    code: "TERMINAL".into(),
                    provider_reference: None,
                },
                BatchJobStatus::Failed,
            ),
            (
                RecognitionFailure::UnknownAfterSend {
                    code: "UNKNOWN".into(),
                    provider_reference: None,
                },
                BatchJobStatus::OutcomeUnknown,
            ),
        ];
        for (failure, status) in cases {
            let (actual, retry_after) = outcome_state(Err(failure));
            assert_eq!(actual, status);
            assert_eq!(
                retry_after,
                (status == BatchJobStatus::Retryable).then_some(250)
            );
        }
    }

    #[test]
    fn invalid_result_identity_timing_and_bounds_never_complete() {
        assert_eq!(
            outcome_state(Ok(result())),
            (BatchJobStatus::Completed, None)
        );
        let mut invalid = [result(), result(), result()];
        invalid[0].profile = BatchProfile::new(2, "other", "model", "multi").unwrap();
        invalid[1].duration_millis = 101;
        invalid[2].segments[0].end_millis = 101;
        for invalid in invalid {
            assert_eq!(outcome_state(Ok(invalid)).0, BatchJobStatus::Failed);
        }

        let mut boundary = result();
        boundary.text = "x".repeat(MAX_TRANSCRIPT_BYTES);
        boundary.segments.clear();
        assert_eq!(outcome_state(Ok(boundary)).0, BatchJobStatus::Completed);

        let mut over_content_budget = result();
        over_content_budget.text = "x".repeat(MAX_TRANSCRIPT_BYTES + 1);
        over_content_budget.segments.clear();
        assert_eq!(
            outcome_state(Ok(over_content_budget)).0,
            BatchJobStatus::Failed
        );
    }
}
