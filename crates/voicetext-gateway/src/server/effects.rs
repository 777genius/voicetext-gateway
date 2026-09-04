//! Shared observation boundary around durably fenced batch provider effects.

use std::sync::Mutex;

use tokio::time::timeout;
use uuid::Uuid;

use voicetext_speech::application::batch::{
    BatchCoordinator, BatchCoordinatorFailure, BatchExecutionOutcome,
};
use voicetext_speech::application::batch_capabilities::BatchCapabilityDescriptor;
use voicetext_speech::application::ports::{
    BatchAudioRemoveOutcome, BatchJobId, BatchRecognitionRequest, BatchRecognitionResult,
    BatchRecognizer, BoxFuture, ProviderReference, RecognitionFailure,
};
use voicetext_speech::domain::batch::BatchJobState;

use crate::contracts::batch::BatchIdentity;
use crate::contracts::batch_projection::GatewayBatchResultProjection;

use super::qualification_observation::{
    BatchObservation, OBSERVATION_WRITE_TIMEOUT, ObservationProfile, OperationObservation,
    batch_result_digest, unix_millis,
};
use super::state::GatewayState;

#[derive(Debug)]
pub(crate) struct FencedExecution {
    pub(crate) outcome: Result<BatchExecutionOutcome, BatchCoordinatorFailure>,
    terminal_cleanup: TerminalCleanup,
}

impl FencedExecution {
    pub(crate) const fn terminal_audio_removed(&self) -> bool {
        matches!(self.terminal_cleanup, TerminalCleanup::Removed)
    }
}

#[derive(Debug)]
enum TerminalCleanup {
    NotRequired,
    Removed,
    AlreadyMissing,
    Retained,
}

pub(crate) async fn execute_fenced(
    state: &GatewayState,
    identity: BatchIdentity,
    id: &BatchJobId,
) -> Option<FencedExecution> {
    let recognizer = state.profiles().batch(identity)?;
    let capture = Mutex::new(None);
    let observed = EffectObservedRecognizer {
        state,
        recognizer: recognizer.as_ref(),
        capture: &capture,
    };
    let coordinator = BatchCoordinator::new(&observed, state.jobs(), state.spool());
    let execution = coordinator
        .execute_with_cleanup_report(id, &GatewayBatchResultProjection)
        .await;
    let (outcome, terminal_cleanup, cleanup_failure) = match execution {
        Ok(report) => {
            let terminal_cleanup = match (&report.outcome, &report.post_persistence_cleanup) {
                (
                    BatchExecutionOutcome::Persisted(snapshot),
                    Some(Ok(BatchAudioRemoveOutcome::Removed)),
                ) if snapshot.job.state().is_terminal() => TerminalCleanup::Removed,
                (
                    BatchExecutionOutcome::Persisted(snapshot),
                    Some(Ok(BatchAudioRemoveOutcome::AlreadyMissing)),
                ) if snapshot.job.state().is_terminal() => TerminalCleanup::AlreadyMissing,
                (BatchExecutionOutcome::Persisted(snapshot), Some(Err(_)))
                    if snapshot.job.state().is_terminal() =>
                {
                    TerminalCleanup::Retained
                }
                _ => TerminalCleanup::NotRequired,
            };
            let cleanup_failure = report.post_persistence_cleanup.and_then(Result::err);
            (Ok(report.outcome), terminal_cleanup, cleanup_failure)
        }
        Err(failure) => (Err(failure), TerminalCleanup::NotRequired, None),
    };
    observe_outcome(state, &outcome);
    if let Some(failure) = cleanup_failure {
        tracing::warn!(
            job_id = %id.as_str(),
            ?failure,
            "batch outcome persisted but terminal spool cleanup failed"
        );
    }
    let capture = capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(capture) = capture {
        observe_qualification(state, id, capture, &outcome).await;
    }
    Some(FencedExecution {
        outcome,
        terminal_cleanup,
    })
}

struct EffectObservedRecognizer<'a> {
    state: &'a GatewayState,
    recognizer: &'a dyn BatchRecognizer,
    capture: &'a Mutex<Option<BatchEffectCapture>>,
}

struct BatchEffectCapture {
    effect_id: Uuid,
    profile: ObservationProfile,
    provider_operation: Option<OperationObservation>,
    result_digest: Option<String>,
    started_at_unix_ms: u64,
    finished_at_unix_ms: u64,
}

impl BatchRecognizer for EffectObservedRecognizer<'_> {
    fn capabilities(&self) -> &'static BatchCapabilityDescriptor {
        self.recognizer.capabilities()
    }

    fn recognize(
        &self,
        request: BatchRecognitionRequest,
    ) -> BoxFuture<'_, Result<BatchRecognitionResult, RecognitionFailure>> {
        self.state.metrics().batch_provider_effect_started();
        Box::pin(async move {
            let started_at_unix_ms = unix_millis();
            let effect_id = Uuid::new_v4();
            let profile = ObservationProfile {
                contract_version: request.profile.contract_version(),
                provider: request.profile.provider().into(),
                model: request.profile.model().into(),
                language: request.profile.language().into(),
            };
            let result = self.recognizer.recognize(request).await;
            let provider_operation = match &result {
                Ok(result) => result.provider_reference.as_ref(),
                Err(failure) => failure.provider_reference(),
            }
            .and_then(ProviderReference::provider_operation)
            .map(Into::into);
            let result_digest = result.as_ref().ok().map(batch_result_digest);
            *self
                .capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(BatchEffectCapture {
                effect_id,
                profile,
                provider_operation,
                result_digest,
                started_at_unix_ms,
                finished_at_unix_ms: unix_millis(),
            });
            result
        })
    }
}

async fn observe_qualification(
    state: &GatewayState,
    id: &BatchJobId,
    capture: BatchEffectCapture,
    outcome: &Result<BatchExecutionOutcome, BatchCoordinatorFailure>,
) {
    if !state.batch_observations().enabled() {
        return;
    }
    let (terminal_status, durable_persistence) = classify_outcome(outcome);
    let record = BatchObservation {
        schema: "voicetext-qualification-observation-v1",
        effect_id: capture.effect_id,
        gateway_job_id: id.as_str().into(),
        profile: capture.profile,
        provider_operation: capture.provider_operation,
        result_digest: capture.result_digest,
        started_at_unix_ms: capture.started_at_unix_ms,
        finished_at_unix_ms: capture.finished_at_unix_ms,
        terminal_status,
        durable_persistence,
    };
    match timeout(
        OBSERVATION_WRITE_TIMEOUT,
        state.batch_observations().observe_batch(record),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(failure)) => observation_failure(state, failure.0),
        Err(_) => observation_failure(state, "QUALIFICATION_WRITE_TIMEOUT"),
    }
}

fn classify_outcome(
    outcome: &Result<BatchExecutionOutcome, BatchCoordinatorFailure>,
) -> (&'static str, &'static str) {
    match outcome {
        Ok(BatchExecutionOutcome::Persisted(snapshot)) => (
            match snapshot.job.state() {
                BatchJobState::Completed { .. } => "completed",
                BatchJobState::Retryable { .. } => "retryable",
                BatchJobState::Failed { .. } => "failed",
                BatchJobState::OutcomeUnknown { .. } => "outcome_unknown",
                BatchJobState::Accepted | BatchJobState::Submitting { .. } => "nonterminal",
            },
            "established",
        ),
        Err(BatchCoordinatorFailure::PostEgressStore(_)) => {
            ("persistence_failed", "not_established")
        }
        Err(BatchCoordinatorFailure::PostEgressConflict) => {
            ("persistence_conflict", "not_established")
        }
        Ok(BatchExecutionOutcome::NotClaimed(_) | BatchExecutionOutcome::NotActionable(_))
        | Err(_) => ("orchestration_failed", "not_established"),
    }
}

fn observation_failure(state: &GatewayState, code: &'static str) {
    state.metrics().qualification_observation_failure();
    tracing::warn!(code, "qualification observation missing");
}

fn observe_outcome(
    state: &GatewayState,
    outcome: &Result<BatchExecutionOutcome, BatchCoordinatorFailure>,
) {
    match outcome {
        Ok(BatchExecutionOutcome::Persisted(snapshot)) => {
            state.metrics().batch_provider_effect_persisted();
            match snapshot.job.state() {
                BatchJobState::Retryable { .. } => {
                    state.metrics().batch_retryable_outcome();
                }
                BatchJobState::Failed { .. } => {
                    state.metrics().batch_known_terminal_failure();
                }
                BatchJobState::OutcomeUnknown { .. } => {
                    state.metrics().batch_outcome_unknown();
                }
                _ => {}
            }
        }
        Err(
            BatchCoordinatorFailure::PostEgressStore(_)
            | BatchCoordinatorFailure::PostEgressConflict,
        ) => state.metrics().batch_provider_effect_persistence_unknown(),
        Ok(BatchExecutionOutcome::NotClaimed(_) | BatchExecutionOutcome::NotActionable(_))
        | Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voicetext_speech::application::ports::BatchJobStoreFailure;

    #[test]
    fn post_egress_persistence_failures_are_never_classified_as_durable() {
        let store = Err(BatchCoordinatorFailure::PostEgressStore(
            BatchJobStoreFailure::Unavailable {
                code: "SYNTHETIC_STORE_FAILURE".into(),
            },
        ));
        assert_eq!(
            classify_outcome(&store),
            ("persistence_failed", "not_established")
        );
        assert_eq!(
            classify_outcome(&Err(BatchCoordinatorFailure::PostEgressConflict)),
            ("persistence_conflict", "not_established")
        );
    }
}
