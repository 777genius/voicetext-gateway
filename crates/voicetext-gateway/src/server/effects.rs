//! Shared observation boundary around durably fenced batch provider effects.

use voicetext_speech::application::batch::{
    BatchCoordinator, BatchCoordinatorFailure, BatchExecutionOutcome,
};
use voicetext_speech::application::batch_capabilities::BatchCapabilityDescriptor;
use voicetext_speech::application::ports::{
    BatchJobId, BatchRecognitionRequest, BatchRecognitionResult, BatchRecognizer, BoxFuture,
    RecognitionFailure,
};
use voicetext_speech::domain::batch::BatchJobState;

use crate::contracts::batch::BatchIdentity;

use super::state::GatewayState;

pub(crate) async fn execute_fenced(
    state: &GatewayState,
    identity: BatchIdentity,
    id: &BatchJobId,
) -> Option<Result<BatchExecutionOutcome, BatchCoordinatorFailure>> {
    let recognizer = state.profiles().batch(identity)?;
    let observed = EffectObservedRecognizer { state, recognizer };
    let coordinator = BatchCoordinator::new(&observed, state.jobs(), state.spool());
    let outcome = coordinator.execute(id).await;
    observe_outcome(state, &outcome);
    Some(outcome)
}

struct EffectObservedRecognizer<'a> {
    state: &'a GatewayState,
    recognizer: &'a dyn BatchRecognizer,
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
        self.recognizer.recognize(request)
    }
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
