//! Cloneable gateway dependencies and bounded transport settings.

use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use voicetext_speech::application::ports::{BatchAudioSpool, BatchJobStore, BoxFuture};

use crate::contracts::batch::BatchIdentity;
use crate::profiles::ProfileRegistry;
use crate::secret::MachineSecret;

use super::metrics::GatewayMetrics;

const MAX_BATCH_UPLOAD_BYTES: usize = 64 * 1_024 * 1_024;
const MAX_LIVE_FRAME_BYTES: usize = 64 * 1_024;
const MAX_CONNECTIONS: usize = 10_000;

/// Process-local transport bounds validated once during composition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GatewayLimits {
    pub batch_upload_bytes: usize,
    pub live_frame_bytes: usize,
    pub live_connections: NonZeroUsize,
    pub first_frame_timeout: Duration,
    pub finalize_timeout: Duration,
}

impl GatewayLimits {
    /// Validates all resource bounds before the router can accept traffic.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidGatewayLimits`] for zero or compatibility-breaking values.
    pub fn new(
        batch_upload_bytes: usize,
        live_frame_bytes: usize,
        live_connections: NonZeroUsize,
        first_frame_timeout: Duration,
        finalize_timeout: Duration,
    ) -> Result<Self, InvalidGatewayLimits> {
        if !(27..=MAX_BATCH_UPLOAD_BYTES).contains(&batch_upload_bytes) {
            return Err(InvalidGatewayLimits::BatchUploadBytes);
        }
        if !(2..=MAX_LIVE_FRAME_BYTES).contains(&live_frame_bytes) {
            return Err(InvalidGatewayLimits::LiveFrameBytes);
        }
        if live_connections.get() > MAX_CONNECTIONS {
            return Err(InvalidGatewayLimits::LiveConnections);
        }
        if !(Duration::from_millis(100)..=Duration::from_mins(2)).contains(&first_frame_timeout) {
            return Err(InvalidGatewayLimits::FirstFrameTimeout);
        }
        if !(Duration::from_millis(100)..=Duration::from_mins(5)).contains(&finalize_timeout) {
            return Err(InvalidGatewayLimits::FinalizeTimeout);
        }
        Ok(Self {
            batch_upload_bytes,
            live_frame_bytes,
            live_connections,
            first_frame_timeout,
            finalize_timeout,
        })
    }
}

/// Invalid composition-time transport limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidGatewayLimits {
    #[error("batch upload bound is invalid")]
    BatchUploadBytes,
    #[error("live frame bound is invalid")]
    LiveFrameBytes,
    #[error("connection bound is invalid")]
    LiveConnections,
    #[error("first frame timeout is invalid")]
    FirstFrameTimeout,
    #[error("finalize timeout is invalid")]
    FinalizeTimeout,
}

/// Replaceable operational readiness probe used by production and black-box tests.
pub trait GatewayReadiness: Send + Sync {
    /// Checks all dependencies required before accepting new work.
    fn check(&self) -> BoxFuture<'_, Result<(), ReadinessFailure>>;
}

/// Safe readiness failure code; dependency details stay in structured logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessFailure {
    code: Box<str>,
}

impl ReadinessFailure {
    /// Creates a bounded uppercase operational code.
    #[must_use]
    pub fn new(code: &'static str) -> Self {
        Self { code: code.into() }
    }

    /// Returns the non-secret operational code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }
}

/// Shared dependencies for one running gateway process.
#[derive(Clone)]
pub struct GatewayState(Arc<GatewayStateInner>);

struct GatewayStateInner {
    auth: MachineSecret,
    jobs: Arc<dyn BatchJobStore>,
    spool: Arc<dyn BatchAudioSpool>,
    profiles: ProfileRegistry,
    readiness: Arc<dyn GatewayReadiness>,
    limits: GatewayLimits,
    live_slots: Arc<Semaphore>,
    global_batch_slots: Arc<Semaphore>,
    batch_capacity: u32,
    batch_slots: Arc<Semaphore>,
    deepgram_batch_slots: Arc<Semaphore>,
    elevenlabs_batch_slots: Arc<Semaphore>,
    batch_tasks: Mutex<Vec<JoinHandle<()>>>,
    batch_task_registration_closed: AtomicBool,
    metrics: GatewayMetrics,
    startup_reconciled: AtomicBool,
    accepting_work: AtomicBool,
}

impl GatewayState {
    /// Composes transport state from narrow replaceable capabilities.
    #[must_use]
    pub fn new(
        auth: MachineSecret,
        jobs: Arc<dyn BatchJobStore>,
        spool: Arc<dyn BatchAudioSpool>,
        profiles: ProfileRegistry,
        readiness: Arc<dyn GatewayReadiness>,
        limits: GatewayLimits,
    ) -> Self {
        let live_slots = Arc::new(Semaphore::new(limits.live_connections.get()));
        let batch_capacity = limits.live_connections.get();
        let batch_capacity_u32 = u32::try_from(batch_capacity).expect("validated batch capacity");
        let provider_capacity = batch_capacity.div_ceil(2).max(1);
        Self(Arc::new(GatewayStateInner {
            auth,
            jobs,
            spool,
            profiles,
            readiness,
            limits,
            live_slots,
            global_batch_slots: Arc::new(Semaphore::new(batch_capacity)),
            batch_capacity: batch_capacity_u32,
            batch_slots: Arc::new(Semaphore::new(batch_capacity)),
            deepgram_batch_slots: Arc::new(Semaphore::new(provider_capacity)),
            elevenlabs_batch_slots: Arc::new(Semaphore::new(provider_capacity)),
            batch_tasks: Mutex::new(Vec::new()),
            batch_task_registration_closed: AtomicBool::new(false),
            metrics: GatewayMetrics::default(),
            startup_reconciled: AtomicBool::new(false),
            accepting_work: AtomicBool::new(true),
        }))
    }

    pub(crate) fn auth(&self) -> &MachineSecret {
        &self.0.auth
    }

    pub(crate) fn jobs(&self) -> &dyn BatchJobStore {
        self.0.jobs.as_ref()
    }

    pub(crate) fn spool(&self) -> &dyn BatchAudioSpool {
        self.0.spool.as_ref()
    }

    pub(crate) fn profiles(&self) -> &ProfileRegistry {
        &self.0.profiles
    }

    pub(crate) fn readiness(&self) -> &dyn GatewayReadiness {
        self.0.readiness.as_ref()
    }

    pub(crate) fn limits(&self) -> GatewayLimits {
        self.0.limits
    }

    pub(crate) fn try_acquire_live_slot(&self) -> Option<OwnedSemaphorePermit> {
        if !self.accepting_work() {
            return None;
        }
        let permit = Arc::clone(&self.0.live_slots).try_acquire_owned().ok()?;
        self.accepting_work().then_some(permit)
    }

    pub(crate) fn try_acquire_global_batch_slot(&self) -> Option<OwnedSemaphorePermit> {
        if !self.accepting_work() {
            return None;
        }
        let permit = Arc::clone(&self.0.global_batch_slots)
            .try_acquire_owned()
            .ok()?;
        self.accepting_work().then_some(permit)
    }

    pub(crate) fn try_acquire_batch_slot(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.0.batch_slots).try_acquire_owned().ok()
    }

    pub(crate) fn try_acquire_provider_batch_slot(
        &self,
        identity: BatchIdentity,
    ) -> Option<OwnedSemaphorePermit> {
        let slots = match identity {
            BatchIdentity::DeepgramNova3MultiV2 => &self.0.deepgram_batch_slots,
            BatchIdentity::ElevenlabsScribeV2MultiV3 => &self.0.elevenlabs_batch_slots,
        };
        Arc::clone(slots).try_acquire_owned().ok()
    }

    pub(crate) async fn acquire_recovery_batch_slots(
        &self,
        identity: BatchIdentity,
    ) -> Option<(
        OwnedSemaphorePermit,
        OwnedSemaphorePermit,
        OwnedSemaphorePermit,
    )> {
        if !self.accepting_work() {
            return None;
        }
        let global = Arc::clone(&self.0.global_batch_slots)
            .acquire_owned()
            .await
            .ok()?;
        let batch = Arc::clone(&self.0.batch_slots).acquire_owned().await.ok()?;
        let provider = match identity {
            BatchIdentity::DeepgramNova3MultiV2 => &self.0.deepgram_batch_slots,
            BatchIdentity::ElevenlabsScribeV2MultiV3 => &self.0.elevenlabs_batch_slots,
        };
        let provider = Arc::clone(provider).acquire_owned().await.ok()?;
        if !self.accepting_work() {
            return None;
        }
        Some((global, batch, provider))
    }

    pub(crate) fn spawn_batch_task(&self, task: impl Future<Output = ()> + Send + 'static) -> bool {
        let mut tasks = self
            .0
            .batch_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self
            .0
            .batch_task_registration_closed
            .load(Ordering::Acquire)
        {
            return false;
        }
        let handle = tokio::spawn(task);
        tasks.retain(|task| !task.is_finished());
        tasks.push(handle);
        true
    }

    /// Stops admission immediately, drains tracked batch tasks until one shared deadline, then
    /// aborts only work that outlives that bound. Returns the number forced to abort.
    pub async fn shutdown_batch_tasks(&self, deadline: tokio::time::Instant) -> usize {
        self.begin_shutdown();
        let all_slots =
            Arc::clone(&self.0.global_batch_slots).acquire_many_owned(self.0.batch_capacity);
        let quiescence_permit = match tokio::time::timeout_at(deadline, all_slots).await {
            Ok(Ok(permit)) => Some(permit),
            Ok(Err(_)) | Err(_) => None,
        };
        self.0
            .batch_task_registration_closed
            .store(true, Ordering::Release);
        let tasks = {
            let mut tracked = self
                .0
                .batch_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *tracked)
        };
        let aborted = drain_tasks_until(tasks, deadline).await;
        drop(quiescence_permit);
        aborted
    }

    pub(crate) fn metrics(&self) -> &GatewayMetrics {
        &self.0.metrics
    }

    /// Records fixed-cardinality startup recovery and spool capacity evidence.
    pub fn record_startup_metrics(
        &self,
        recovery_executed: u64,
        recovery_unknown: u64,
        terminal_removed: u64,
        orphan_removed: u64,
        spool_used_bytes: u64,
        spool_capacity_bytes: u64,
    ) {
        self.0
            .metrics
            .record_recovery(recovery_executed, recovery_unknown);
        self.0.metrics.record_spool(
            terminal_removed,
            orphan_removed,
            spool_used_bytes,
            spool_capacity_bytes,
        );
    }

    pub(crate) fn mark_startup_reconciled(&self) {
        self.0.startup_reconciled.store(true, Ordering::Release);
    }

    pub(crate) fn startup_reconciled(&self) -> bool {
        self.0.startup_reconciled.load(Ordering::Acquire)
    }

    /// Stops new batch/live admission and makes readiness fail before draining begins.
    pub fn begin_shutdown(&self) {
        self.0.accepting_work.store(false, Ordering::Release);
    }

    pub(crate) fn accepting_work(&self) -> bool {
        self.0.accepting_work.load(Ordering::Acquire)
    }
}

#[cfg(test)]
async fn drain_tasks(tasks: Vec<JoinHandle<()>>, maximum: Duration) -> usize {
    drain_tasks_until(tasks, tokio::time::Instant::now() + maximum).await
}

async fn drain_tasks_until(
    mut tasks: Vec<JoinHandle<()>>,
    deadline: tokio::time::Instant,
) -> usize {
    while let Some(task) = tasks.first_mut() {
        if tokio::time::timeout_at(deadline, task).await.is_err() {
            break;
        }
        drop(tasks.swap_remove(0));
    }
    tasks.retain(|task| !task.is_finished());
    let aborted = tasks.len();
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _joined = task.await;
    }
    aborted
}

impl fmt::Debug for GatewayState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayState")
            .field("auth", &"[REDACTED]")
            .field("profiles", &self.0.profiles)
            .field("limits", &self.0.limits)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn limits_reject_unbounded_values() {
        let connections = NonZeroUsize::new(1).unwrap();
        assert!(
            GatewayLimits::new(
                MAX_BATCH_UPLOAD_BYTES + 1,
                1_275,
                connections,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .is_err()
        );
        assert!(
            GatewayLimits::new(
                MAX_BATCH_UPLOAD_BYTES,
                1_275,
                NonZeroUsize::new(MAX_CONNECTIONS + 1).unwrap(),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .is_err()
        );
        assert!(
            GatewayLimits::new(
                MAX_BATCH_UPLOAD_BYTES,
                1_275,
                connections,
                Duration::from_millis(99),
                Duration::from_secs(1),
            )
            .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sigterm_drain_preserves_completed_work_until_the_shared_deadline() {
        let completed = Arc::new(AtomicUsize::new(0));
        let task_completed = Arc::clone(&completed);
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _previous = task_completed.fetch_add(1, Ordering::SeqCst);
        });
        let drain = tokio::spawn(drain_tasks(vec![task], Duration::from_secs(3)));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(drain.await.unwrap(), 0);
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn sigterm_drain_aborts_only_when_the_shared_deadline_expires() {
        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);
        let task = tokio::spawn(async move {
            struct DropEvidence(Arc<AtomicBool>);
            impl Drop for DropEvidence {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _evidence = DropEvidence(task_dropped);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        let drain = tokio::spawn(drain_tasks(vec![task], Duration::from_secs(3)));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!drain.is_finished());
        assert!(!dropped.load(Ordering::SeqCst));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(drain.await.unwrap(), 1);
        assert!(dropped.load(Ordering::SeqCst));
    }
}
