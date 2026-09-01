//! Fixed-cardinality process metrics without request or secret labels.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};

use super::state::GatewayState;

/// Process-local counters suitable for one gateway replica.
#[derive(Debug, Default)]
pub struct GatewayMetrics {
    batch_requests: AtomicU64,
    batch_conflicts: AtomicU64,
    batch_failures: AtomicU64,
    live_sessions: AtomicU64,
    live_frames: AtomicU64,
    live_failures: AtomicU64,
}

impl GatewayMetrics {
    pub(crate) fn batch_request(&self) {
        self.batch_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_conflict(&self) {
        self.batch_conflicts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_failure(&self) {
        self.batch_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn live_session(&self) {
        self.live_sessions.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn live_frame(&self) {
        self.live_frames.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn live_failure(&self) {
        self.live_failures.fetch_add(1, Ordering::Relaxed);
    }

    fn render(&self) -> String {
        let mut output = String::with_capacity(512);
        for (name, help, value) in [
            (
                "voicetext_batch_requests_total",
                "Authenticated batch HTTP requests.",
                self.batch_requests.load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_conflicts_total",
                "Rejected batch idempotency conflicts.",
                self.batch_conflicts.load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_failures_total",
                "Batch transport or orchestration failures.",
                self.batch_failures.load(Ordering::Relaxed),
            ),
            (
                "voicetext_live_sessions_total",
                "Accepted live WebSocket sessions.",
                self.live_sessions.load(Ordering::Relaxed),
            ),
            (
                "voicetext_live_frames_total",
                "Audio frames written to a live provider.",
                self.live_frames.load(Ordering::Relaxed),
            ),
            (
                "voicetext_live_failures_total",
                "Live protocol or provider failures.",
                self.live_failures.load(Ordering::Relaxed),
            ),
        ] {
            let _write = writeln!(output, "# HELP {name} {help}");
            let _write = writeln!(output, "# TYPE {name} counter");
            let _write = writeln!(output, "{name} {value}");
        }
        output
    }
}

pub(crate) async fn metrics(State(state): State<GatewayState>) -> (StatusCode, HeaderMap, String) {
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    (StatusCode::OK, headers, state.metrics().render())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_have_fixed_names_and_no_dynamic_labels() {
        let metrics = GatewayMetrics::default();
        metrics.batch_request();
        metrics.live_frame();
        let rendered = metrics.render();
        assert!(rendered.contains("voicetext_batch_requests_total 1\n"));
        assert!(rendered.contains("voicetext_live_frames_total 1\n"));
        assert!(!rendered.contains('{'));
    }
}
