//! Bounded startup reconciliation for durable pre-egress batch work.

use std::fmt;
use std::num::NonZeroUsize;

use voicetext_speech::application::batch::{
    BatchCoordinator, BatchCoordinatorFailure, BatchExecutionOutcome,
};
use voicetext_speech::application::ports::{BatchJobId, BatchJobSnapshot, BatchRecognizer};
use voicetext_speech::domain::batch::BatchProfile;

use crate::contracts::batch::BatchIdentity;

use super::state::GatewayState;

const RESUME_PAGE_LIMIT: NonZeroUsize = NonZeroUsize::new(100).unwrap();

/// Bounded evidence produced before the listener starts accepting requests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupRecoverySummary {
    pub pages: u64,
    pub recovered_unknown: u64,
    pub executed: u64,
    pub not_claimed: u64,
    pub skipped_unconfigured: u64,
    pub conflicts: u64,
    pub missing: u64,
}

/// Safe startup recovery failure without provider or database response bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupRecoveryFailure {
    code: &'static str,
}

impl StartupRecoveryFailure {
    fn new(code: &'static str) -> Self {
        Self { code }
    }

    /// Stable operational code suitable for a structured startup log.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for StartupRecoveryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for StartupRecoveryFailure {}

/// Reconciles every bounded page and safely resumes only known pre-egress jobs.
///
/// Interrupted `Submitting` jobs become terminally unknown before any new
/// provider call. Readiness is then released before accepted/retryable jobs
/// execute sequentially through their exact configured profile, preventing an
/// unbounded paid startup burst.
///
/// # Errors
///
/// Returns a stable failure when the ledger cannot be reconciled, a cursor does
/// not advance, or a provider effect cannot be durably classified.
pub async fn recover_startup(
    state: &GatewayState,
) -> Result<StartupRecoverySummary, StartupRecoveryFailure> {
    let seed = first_batch_recognizer(state)
        .ok_or_else(|| StartupRecoveryFailure::new("NO_BATCH_PROFILE_CONFIGURED"))?;
    let mut cursor: Option<BatchJobId> = None;
    let mut summary = StartupRecoverySummary::default();

    loop {
        let coordinator = BatchCoordinator::new(seed.as_ref(), state.jobs(), state.spool());
        let page = coordinator
            .recover_startup(cursor.clone())
            .await
            .map_err(|failure| map_failure(&failure))?;
        summary.pages = summary.pages.saturating_add(1);
        summary.recovered_unknown = summary
            .recovered_unknown
            .saturating_add(page.recovered_unknown.len() as u64);
        summary.conflicts = summary
            .conflicts
            .saturating_add(page.conflicts.len() as u64);
        summary.missing = summary.missing.saturating_add(page.missing.len() as u64);

        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        if cursor.as_ref() == Some(&next_cursor) {
            return Err(StartupRecoveryFailure::new(
                "RECOVERY_CURSOR_DID_NOT_ADVANCE",
            ));
        }
        cursor = Some(next_cursor);
    }
    state.mark_startup_reconciled();

    cursor = None;
    loop {
        let candidates = state
            .jobs()
            .list_recovery_candidates(cursor.clone(), RESUME_PAGE_LIMIT)
            .await
            .map_err(|_| StartupRecoveryFailure::new("RECOVERY_LEDGER_UNAVAILABLE"))?;
        let has_next_page = candidates.len() >= RESUME_PAGE_LIMIT.get();
        let next_cursor = candidates.last().map(|snapshot| snapshot.id.clone());
        for snapshot in candidates {
            execute_actionable(state, &snapshot, &mut summary).await?;
        }
        if !has_next_page {
            break;
        }
        let next_cursor =
            next_cursor.ok_or_else(|| StartupRecoveryFailure::new("RECOVERY_CURSOR_MISSING"))?;
        if cursor.as_ref() == Some(&next_cursor) {
            return Err(StartupRecoveryFailure::new(
                "RECOVERY_CURSOR_DID_NOT_ADVANCE",
            ));
        }
        cursor = Some(next_cursor);
    }
    Ok(summary)
}

async fn execute_actionable(
    state: &GatewayState,
    snapshot: &BatchJobSnapshot,
    summary: &mut StartupRecoverySummary,
) -> Result<(), StartupRecoveryFailure> {
    let Some(identity) = identity_for_profile(snapshot.job.profile()) else {
        summary.skipped_unconfigured = summary.skipped_unconfigured.saturating_add(1);
        return Ok(());
    };
    let Some(recognizer) = state.profiles().batch(identity) else {
        summary.skipped_unconfigured = summary.skipped_unconfigured.saturating_add(1);
        return Ok(());
    };
    let coordinator = BatchCoordinator::new(recognizer.as_ref(), state.jobs(), state.spool());
    match coordinator
        .execute(&snapshot.id)
        .await
        .map_err(|failure| map_failure(&failure))?
    {
        BatchExecutionOutcome::Persisted(_) => {
            summary.executed = summary.executed.saturating_add(1);
        }
        BatchExecutionOutcome::NotClaimed(_) | BatchExecutionOutcome::NotActionable(_) => {
            summary.not_claimed = summary.not_claimed.saturating_add(1);
        }
    }
    Ok(())
}

fn first_batch_recognizer(state: &GatewayState) -> Option<&std::sync::Arc<dyn BatchRecognizer>> {
    state
        .profiles()
        .batch(BatchIdentity::DeepgramNova3MultiV2)
        .or_else(|| {
            state
                .profiles()
                .batch(BatchIdentity::ElevenlabsScribeV2MultiV3)
        })
}

fn identity_for_profile(profile: &BatchProfile) -> Option<BatchIdentity> {
    match (
        profile.contract_version(),
        profile.provider(),
        profile.model(),
        profile.language(),
    ) {
        (2, "deepgram", "nova-3", "multi") => Some(BatchIdentity::DeepgramNova3MultiV2),
        (3, "elevenlabs", "scribe_v2", "multi") => Some(BatchIdentity::ElevenlabsScribeV2MultiV3),
        _ => None,
    }
}

fn map_failure(failure: &BatchCoordinatorFailure) -> StartupRecoveryFailure {
    match failure {
        BatchCoordinatorFailure::Spool(_) | BatchCoordinatorFailure::AdmissionCleanup { .. } => {
            StartupRecoveryFailure::new("RECOVERY_SPOOL_UNAVAILABLE")
        }
        BatchCoordinatorFailure::PostEgressStore(_)
        | BatchCoordinatorFailure::PostEgressConflict => {
            StartupRecoveryFailure::new("RECOVERY_POST_EGRESS_UNCERTAIN")
        }
        _ => StartupRecoveryFailure::new("RECOVERY_LEDGER_UNAVAILABLE"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_profiles_are_selected_without_fallback() {
        let deepgram = BatchProfile::new(2, "deepgram", "nova-3", "multi").unwrap();
        let future = BatchProfile::new(4, "future", "model", "multi").unwrap();
        assert_eq!(
            identity_for_profile(&deepgram),
            Some(BatchIdentity::DeepgramNova3MultiV2)
        );
        assert_eq!(identity_for_profile(&future), None);
    }
}
