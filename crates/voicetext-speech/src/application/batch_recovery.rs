//! Deterministic bounded-page reconciliation for durable batch startup state.

use super::batch::{BatchCoordinator, RECOVERY_BATCH_LIMIT, store};
use super::batch_models::{BatchCoordinatorFailure, BatchStartupRecovery};
use super::ports::{BatchJobId, BatchJobUpdateOutcome, BoxFuture};
use crate::domain::batch::BatchJobState;

impl BatchCoordinator<'_> {
    /// Reconciles one legacy unbounded-head page for compatibility callers.
    pub fn recover_startup(
        &self,
        after: Option<BatchJobId>,
    ) -> BoxFuture<'_, Result<BatchStartupRecovery, BatchCoordinatorFailure>> {
        Box::pin(async move {
            let candidates = self
                .jobs
                .list_recovery_candidates(after, RECOVERY_BATCH_LIMIT)
                .await
                .map_err(store)?;
            self.reconcile_candidates(candidates).await
        })
    }

    /// Makes interrupted submissions terminally unknown through an inclusive frozen head.
    pub fn recover_startup_through(
        &self,
        after: Option<BatchJobId>,
        through: BatchJobId,
    ) -> BoxFuture<'_, Result<BatchStartupRecovery, BatchCoordinatorFailure>> {
        Box::pin(async move {
            let candidates = self
                .jobs
                .list_recovery_candidates_through(after, through, RECOVERY_BATCH_LIMIT)
                .await
                .map_err(store)?;
            self.reconcile_candidates(candidates).await
        })
    }

    async fn reconcile_candidates(
        &self,
        mut candidates: Vec<super::ports::BatchJobSnapshot>,
    ) -> Result<BatchStartupRecovery, BatchCoordinatorFailure> {
        let mut report = BatchStartupRecovery::default();
        if candidates.len() >= RECOVERY_BATCH_LIMIT.get() {
            candidates.truncate(RECOVERY_BATCH_LIMIT.get());
            report.next_cursor = candidates.last().map(|snapshot| snapshot.id.clone());
        }
        for snapshot in candidates {
            match snapshot.job.state() {
                BatchJobState::Accepted | BatchJobState::Retryable { .. } => {
                    report.actionable.push(snapshot);
                }
                BatchJobState::Submitting { .. } => {
                    let id = snapshot.id.clone();
                    let expected_revision = snapshot.revision;
                    let mut recovered = snapshot;
                    recovered.job.recover_interrupted_submission();
                    match self
                        .jobs
                        .compare_and_swap(expected_revision, recovered)
                        .await
                        .map_err(store)?
                    {
                        BatchJobUpdateOutcome::Stored(snapshot) => {
                            self.spool
                                .remove(&snapshot.audio)
                                .await
                                .map_err(BatchCoordinatorFailure::Spool)?;
                            report.recovered_unknown.push(snapshot);
                        }
                        BatchJobUpdateOutcome::RevisionConflict(snapshot) => {
                            report.conflicts.push(snapshot);
                        }
                        BatchJobUpdateOutcome::Missing => report.missing.push(id),
                    }
                }
                _ => report.invalid_candidates.push(snapshot),
            }
        }
        Ok(report)
    }
}
