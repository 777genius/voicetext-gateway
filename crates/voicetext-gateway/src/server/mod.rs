//! Runnable HTTP and WebSocket transport adapters.

use std::iter;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::header::AUTHORIZATION;
use axum::routing::{get, post};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::trace::TraceLayer;

mod batch;
mod effects;
mod error;
mod health;
mod live;
mod live_diagnostics;
mod live_error;
mod metrics;
mod recovery;
mod state;

pub use health::PostgresSpoolReadiness;
pub use metrics::GatewayMetrics;
pub use recovery::{
    StartupRecoveryFailure, StartupRecoveryPlan, StartupRecoverySummary, reconcile_startup,
    recover_startup, start_startup_recovery,
};
pub use state::{
    GatewayLimits, GatewayReadiness, GatewayState, InvalidGatewayLimits, ReadinessFailure,
};

/// Builds the complete VoiceText-compatible router with fail-closed body bounds.
pub fn router(state: GatewayState) -> Router {
    const MULTIPART_OVERHEAD_BYTES: usize = 256 * 1_024;
    let request_limit = state
        .limits()
        .batch_upload_bytes
        .saturating_add(MULTIPART_OVERHEAD_BYTES);
    Router::new()
        .route("/api/v1/transcribe/batch", post(batch::post))
        .route("/api/v1/transcribe/batch/{job_id}", get(batch::get))
        .route("/api/v1/transcribe/stream", get(live::upgrade))
        .route("/health", get(health::compatibility))
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .route("/metrics", get(metrics::metrics))
        .layer(DefaultBodyLimit::max(request_limit))
        .layer(TraceLayer::new_for_http())
        .layer(SetSensitiveRequestHeadersLayer::new(iter::once(
            AUTHORIZATION,
        )))
        .layer(CatchPanicLayer::new())
        .with_state(state)
}
