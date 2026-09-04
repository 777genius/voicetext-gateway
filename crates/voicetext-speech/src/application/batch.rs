//! Runtime-independent orchestration for durable batch recognition.

use std::{fmt, num::NonZeroUsize};

use crate::application::batch_models::apply_recognition_outcome;
pub use crate::application::batch_models::{
    BatchAdmissionOutcome, BatchAdmissionRequest, BatchCoordinatorFailure, BatchExecutionOutcome,
    BatchStartupRecovery,
};
use crate::application::ports::{
    BatchAudioHandle, BatchAudioRemoveOutcome, BatchAudioSpool, BatchAudioSpoolFailure,
    BatchAudioStoreOutcome, BatchJobId, BatchJobInsertOutcome, BatchJobSnapshot, BatchJobStore,
    BatchJobStoreFailure, BatchJobUpdateOutcome, BatchRecognitionRequest, BatchRecognizer,
    BatchResultProjection, BoxFuture,
};
use crate::domain::batch::{BatchJob, BatchJobState};

pub(super) const RECOVERY_BATCH_LIMIT: NonZeroUsize = NonZeroUsize::new(100).unwrap();

/// A stored provider outcome and the independent result of terminal audio cleanup.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchExecutionReport {
    /// The ledger outcome, including the exact terminal snapshot when stored.
    pub outcome: BatchExecutionOutcome,
    /// Actual cleanup outcome after the terminal snapshot was stored, if cleanup was required.
    pub post_persistence_cleanup: Option<Result<BatchAudioRemoveOutcome, BatchAudioSpoolFailure>>,
}

/// Coordinates admission, a single fenced provider call, and startup recovery.
pub struct BatchCoordinator<'a> {
    recognizer: &'a dyn BatchRecognizer,
    pub(super) jobs: &'a dyn BatchJobStore,
    pub(super) spool: &'a dyn BatchAudioSpool,
}

impl fmt::Debug for BatchCoordinator<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BatchCoordinator")
            .finish_non_exhaustive()
    }
}

impl<'a> BatchCoordinator<'a> {
    pub const fn new(
        recognizer: &'a dyn BatchRecognizer,
        jobs: &'a dyn BatchJobStore,
        spool: &'a dyn BatchAudioSpool,
    ) -> Self {
        Self {
            recognizer,
            jobs,
            spool,
        }
    }

    pub fn admit(
        &self,
        mut request: BatchAdmissionRequest,
    ) -> BoxFuture<'_, Result<BatchAdmissionOutcome, BatchCoordinatorFailure>> {
        Box::pin(async move {
            if let Some(existing) = self.jobs.load(&request.id).await.map_err(store)? {
                return classify_existing(existing, &request);
            }
            let stored = self
                .spool
                .store(request.id.clone(), std::mem::take(&mut request.audio))
                .await
                .map_err(BatchCoordinatorFailure::Spool)?;
            let (audio, owns_audio) = match stored {
                BatchAudioStoreOutcome::Stored(handle) => (handle, true),
                BatchAudioStoreOutcome::Existing(handle) => (handle, false),
            };
            let insert = self
                .jobs
                .insert(
                    request.id.clone(),
                    BatchJob::accept(request.profile.clone(), request.fingerprint),
                    audio.clone(),
                    request.authoritative_duration_millis,
                    request.keyterms.clone(),
                )
                .await;
            match insert {
                Ok(BatchJobInsertOutcome::Inserted(snapshot)) => {
                    Ok(BatchAdmissionOutcome::Accepted(snapshot))
                }
                Ok(BatchJobInsertOutcome::Existing(existing)) => {
                    self.resolve_race(existing, &request, owns_audio, &audio)
                        .await
                }
                Err(insert) => match self.jobs.load(&request.id).await {
                    Ok(Some(existing)) => {
                        self.resolve_race(existing, &request, owns_audio, &audio)
                            .await
                    }
                    Ok(None) if owns_audio => {
                        self.spool.remove(&audio).await.map_err(|spool| {
                            BatchCoordinatorFailure::AdmissionCleanup {
                                store: Some(insert.clone()),
                                spool,
                            }
                        })?;
                        Err(BatchCoordinatorFailure::Store(insert))
                    }
                    Ok(None) => Err(BatchCoordinatorFailure::Store(insert)),
                    Err(verification) => Err(BatchCoordinatorFailure::AdmissionStoreUncertain {
                        insert,
                        verification,
                    }),
                },
            }
        })
    }

    async fn resolve_race(
        &self,
        existing: BatchJobSnapshot,
        request: &BatchAdmissionRequest,
        owns_audio: bool,
        candidate: &BatchAudioHandle,
    ) -> Result<BatchAdmissionOutcome, BatchCoordinatorFailure> {
        if owns_audio && candidate != &existing.audio {
            self.spool.remove(candidate).await.map_err(|spool| {
                BatchCoordinatorFailure::AdmissionCleanup { store: None, spool }
            })?;
        }
        classify_existing(existing, request)
    }

    /// Claims an actionable state and validates a provider result before durable completion.
    pub fn execute<'b>(
        &'b self,
        id: &BatchJobId,
        projection: &'b dyn BatchResultProjection,
    ) -> BoxFuture<'b, Result<BatchExecutionOutcome, BatchCoordinatorFailure>> {
        let id = id.clone();
        Box::pin(async move {
            self.execute_with_cleanup_report(&id, projection)
                .await
                .map(|report| report.outcome)
        })
    }

    /// Executes once while reporting terminal spool cleanup separately from persistence.
    pub fn execute_with_cleanup_report<'b>(
        &'b self,
        id: &BatchJobId,
        projection: &'b dyn BatchResultProjection,
    ) -> BoxFuture<'b, Result<BatchExecutionReport, BatchCoordinatorFailure>> {
        let id = id.clone();
        Box::pin(async move {
            let Some(snapshot) = self.jobs.load(&id).await.map_err(store)? else {
                return Ok(report(BatchExecutionOutcome::NotClaimed(None), None));
            };
            if !matches!(
                snapshot.job.state(),
                BatchJobState::Accepted | BatchJobState::Retryable { .. }
            ) {
                return Ok(report(BatchExecutionOutcome::NotActionable(snapshot), None));
            }
            let audio = self
                .spool
                .read(&snapshot.audio)
                .await
                .map_err(BatchCoordinatorFailure::Spool)?;
            let expected_revision = snapshot.revision;
            let mut claimed = snapshot;
            claimed
                .job
                .begin_submission()
                .map_err(BatchCoordinatorFailure::Transition)?;
            claimed.provider_reference = None;
            claimed.retry_after_millis = None;
            claimed.result = None;
            let claimed = match self
                .jobs
                .compare_and_swap(expected_revision, claimed)
                .await
                .map_err(store)?
            {
                BatchJobUpdateOutcome::Stored(snapshot) => snapshot,
                BatchJobUpdateOutcome::RevisionConflict(snapshot) => {
                    return Ok(report(
                        BatchExecutionOutcome::NotClaimed(Some(snapshot)),
                        None,
                    ));
                }
                BatchJobUpdateOutcome::Missing => {
                    return Ok(report(BatchExecutionOutcome::NotClaimed(None), None));
                }
            };
            let recognition = self
                .recognizer
                .recognize(BatchRecognitionRequest {
                    profile: claimed.job.profile().clone(),
                    audio,
                    authoritative_duration_millis: claimed.authoritative_duration_millis,
                    keyterms: claimed.keyterms.clone(),
                })
                .await;
            let replacement = apply_recognition_outcome(claimed.clone(), recognition, projection)
                .map_err(BatchCoordinatorFailure::Transition)?;
            match self
                .jobs
                .compare_and_swap(claimed.revision, replacement)
                .await
            {
                Ok(BatchJobUpdateOutcome::Stored(snapshot)) => {
                    let cleanup = if snapshot.job.state().is_terminal() {
                        Some(self.spool.remove(&snapshot.audio).await)
                    } else {
                        None
                    };
                    Ok(report(BatchExecutionOutcome::Persisted(snapshot), cleanup))
                }
                Ok(BatchJobUpdateOutcome::RevisionConflict(_) | BatchJobUpdateOutcome::Missing) => {
                    Err(BatchCoordinatorFailure::PostEgressConflict)
                }
                Err(failure) => Err(BatchCoordinatorFailure::PostEgressStore(failure)),
            }
        })
    }
}

fn report(
    outcome: BatchExecutionOutcome,
    post_persistence_cleanup: Option<Result<BatchAudioRemoveOutcome, BatchAudioSpoolFailure>>,
) -> BatchExecutionReport {
    BatchExecutionReport {
        outcome,
        post_persistence_cleanup,
    }
}

fn classify_existing(
    existing: BatchJobSnapshot,
    request: &BatchAdmissionRequest,
) -> Result<BatchAdmissionOutcome, BatchCoordinatorFailure> {
    if existing.job.profile() == &request.profile
        && existing.job.fingerprint() == request.fingerprint
    {
        Ok(BatchAdmissionOutcome::Replay(existing))
    } else {
        Err(BatchCoordinatorFailure::AdmissionConflict(Box::new(
            existing,
        )))
    }
}

pub(super) fn store(failure: BatchJobStoreFailure) -> BatchCoordinatorFailure {
    BatchCoordinatorFailure::Store(failure)
}

#[cfg(test)]
mod tests;
