//! Cloneable gateway dependencies and bounded transport settings.

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use voicetext_speech::application::ports::{BatchAudioSpool, BatchJobStore, BoxFuture};

use crate::profiles::ProfileRegistry;
use crate::secret::MachineSecret;

use super::metrics::GatewayMetrics;

const MAX_BATCH_UPLOAD_BYTES: usize = 64 * 1_024 * 1_024;
const MAX_LIVE_FRAME_BYTES: usize = 64 * 1_024;

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
    metrics: GatewayMetrics,
    startup_reconciled: AtomicBool,
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
        Self(Arc::new(GatewayStateInner {
            auth,
            jobs,
            spool,
            profiles,
            readiness,
            limits,
            live_slots,
            metrics: GatewayMetrics::default(),
            startup_reconciled: AtomicBool::new(false),
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
        Arc::clone(&self.0.live_slots).try_acquire_owned().ok()
    }

    pub(crate) fn metrics(&self) -> &GatewayMetrics {
        &self.0.metrics
    }

    pub(crate) fn mark_startup_reconciled(&self) {
        self.0.startup_reconciled.store(true, Ordering::Release);
    }

    pub(crate) fn startup_reconciled(&self) -> bool {
        self.0.startup_reconciled.load(Ordering::Acquire)
    }
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
                connections,
                Duration::from_millis(99),
                Duration::from_secs(1),
            )
            .is_err()
        );
    }
}
