//! Exact-head startup reconciliation and bounded background backlog recovery.

use std::fmt;
use std::num::NonZeroUsize;

use voicetext_speech::application::batch::{
    BatchCoordinator, BatchCoordinatorFailure, BatchExecutionOutcome,
};
use voicetext_speech::application::ports::{BatchJobId, BatchJobSnapshot, BatchRecognizer};
use voicetext_speech::domain::batch::BatchProfile;

use crate::contracts::batch::BatchIdentity;

use super::effects::execute_fenced;
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

/// One exact startup backlog, frozen before reconciliation begins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupRecoveryPlan {
    pub summary: StartupRecoverySummary,
    head: Option<BatchJobId>,
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

/// Reconciles interrupted submissions through one exact startup head.
///
/// No provider call occurs here. Every `Submitting` row visible at the frozen
/// head is made terminally unknown before readiness can become true.
///
/// # Errors
///
/// Returns a stable failure when the ledger cannot be scanned or reconciled.
pub async fn reconcile_startup(
    state: &GatewayState,
) -> Result<StartupRecoveryPlan, StartupRecoveryFailure> {
    let head = state
        .jobs()
        .recovery_head()
        .await
        .map_err(|_| StartupRecoveryFailure::new("RECOVERY_LEDGER_UNAVAILABLE"))?;
    let mut summary = StartupRecoverySummary::default();
    let Some(through) = head.clone() else {
        state.mark_startup_reconciled();
        return Ok(StartupRecoveryPlan { summary, head });
    };
    let seed = first_batch_recognizer(state)
        .ok_or_else(|| StartupRecoveryFailure::new("NO_BATCH_PROFILE_CONFIGURED"))?;
    let mut cursor: Option<BatchJobId> = None;
    loop {
        let coordinator = BatchCoordinator::new(seed.as_ref(), state.jobs(), state.spool());
        let page = coordinator
            .recover_startup_through(cursor.clone(), through.clone())
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
        advance_cursor(cursor.as_ref(), &next_cursor)?;
        cursor = Some(next_cursor);
    }
    state.mark_startup_reconciled();
    Ok(StartupRecoveryPlan { summary, head })
}

/// Starts the cancellable, admission-bounded worker for the frozen backlog.
pub fn start_startup_recovery(state: &GatewayState, plan: StartupRecoveryPlan) {
    let Some(head) = plan.head else {
        return;
    };
    let task_state = state.clone();
    state.spawn_batch_task(async move {
        if let Err(failure) = drain_backlog(&task_state, head).await {
            task_state.metrics().batch_failure();
            tracing::error!(code = failure.code(), "startup backlog recovery stopped");
        }
    });
}

/// Reconciles startup state and schedules its bounded backlog for compatibility callers.
///
/// # Errors
///
/// Returns the same bounded reconciliation failures as [`reconcile_startup`].
pub async fn recover_startup(
    state: &GatewayState,
) -> Result<StartupRecoverySummary, StartupRecoveryFailure> {
    let plan = reconcile_startup(state).await?;
    let summary = plan.summary.clone();
    start_startup_recovery(state, plan);
    Ok(summary)
}

async fn drain_backlog(
    state: &GatewayState,
    head: BatchJobId,
) -> Result<(), StartupRecoveryFailure> {
    let mut cursor: Option<BatchJobId> = None;
    loop {
        let candidates = state
            .jobs()
            .list_recovery_candidates_through(cursor.clone(), head.clone(), RESUME_PAGE_LIMIT)
            .await
            .map_err(|_| StartupRecoveryFailure::new("RECOVERY_LEDGER_UNAVAILABLE"))?;
        let has_next_page = candidates.len() >= RESUME_PAGE_LIMIT.get();
        let next_cursor = candidates.last().map(|snapshot| snapshot.id.clone());
        for snapshot in candidates {
            execute_actionable(state, &snapshot).await;
        }
        if !has_next_page {
            return Ok(());
        }
        let next_cursor =
            next_cursor.ok_or_else(|| StartupRecoveryFailure::new("RECOVERY_CURSOR_MISSING"))?;
        advance_cursor(cursor.as_ref(), &next_cursor)?;
        cursor = Some(next_cursor);
    }
}

async fn execute_actionable(state: &GatewayState, snapshot: &BatchJobSnapshot) {
    let Some(identity) = identity_for_profile(snapshot.job.profile()) else {
        return;
    };
    if state.profiles().batch(identity).is_none() {
        return;
    }
    let Some(_permits) = state.acquire_recovery_batch_slots(identity).await else {
        return;
    };
    let audio_bytes = state
        .spool()
        .read(&snapshot.audio)
        .await
        .ok()
        .and_then(|audio| u64::try_from(audio.len()).ok());
    state.metrics().batch_execution_started();
    let outcome = execute_fenced(state, identity, &snapshot.id).await;
    state.metrics().batch_execution_finished();
    match outcome {
        Some(Ok(BatchExecutionOutcome::Persisted(snapshot))) => {
            state.metrics().batch_recovery_executed();
            if snapshot.job.state().is_terminal()
                && let Some(audio_bytes) = audio_bytes
            {
                state.metrics().spool_terminal_cleaned(audio_bytes);
            }
        }
        Some(Ok(
            BatchExecutionOutcome::NotClaimed(_) | BatchExecutionOutcome::NotActionable(_),
        ))
        | None => {}
        Some(Err(_)) => {
            state.metrics().batch_failure();
            tracing::error!(job_id = %snapshot.id.as_str(), "recovery execution did not persist a safe outcome");
        }
    }
}

fn advance_cursor(
    current: Option<&BatchJobId>,
    next: &BatchJobId,
) -> Result<(), StartupRecoveryFailure> {
    if current == Some(next) {
        return Err(StartupRecoveryFailure::new(
            "RECOVERY_CURSOR_DID_NOT_ADVANCE",
        ));
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
