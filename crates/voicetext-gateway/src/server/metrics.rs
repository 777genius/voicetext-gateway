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
    batch_admission_rejections: AtomicU64,
    batch_inflight: AtomicU64,
    batch_provider_effects: AtomicU64,
    batch_provider_effects_persisted: AtomicU64,
    batch_provider_effects_persistence_unknown: AtomicU64,
    batch_outcome_unknown: AtomicU64,
    batch_known_terminal_failures: AtomicU64,
    batch_retryable_outcomes: AtomicU64,
    batch_recovery_executed: AtomicU64,
    batch_recovery_unknown: AtomicU64,
    spool_terminal_removed: AtomicU64,
    spool_orphan_removed: AtomicU64,
    spool_used_bytes: AtomicU64,
    spool_capacity_bytes: AtomicU64,
    live_sessions: AtomicU64,
    live_frames: AtomicU64,
    live_failures: AtomicU64,
    qualification_observation_failures: AtomicU64,
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

    pub(crate) fn batch_admission_rejection(&self) {
        self.batch_admission_rejections
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_execution_started(&self) {
        self.batch_inflight.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_execution_finished(&self) {
        self.batch_inflight.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_provider_effect_started(&self) {
        self.batch_provider_effects.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_provider_effect_persisted(&self) {
        self.batch_provider_effects_persisted
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_provider_effect_persistence_unknown(&self) {
        self.batch_provider_effects_persistence_unknown
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_outcome_unknown(&self) {
        self.batch_outcome_unknown.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_known_terminal_failure(&self) {
        self.batch_known_terminal_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_retryable_outcome(&self) {
        self.batch_retryable_outcomes
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_recovery_executed(&self) {
        self.batch_recovery_executed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_recovery(&self, executed: u64, unknown: u64) {
        self.batch_recovery_executed
            .fetch_add(executed, Ordering::Relaxed);
        self.batch_recovery_unknown
            .fetch_add(unknown, Ordering::Relaxed);
    }

    pub(crate) fn record_spool(
        &self,
        terminal_removed: u64,
        orphan_removed: u64,
        used_bytes: u64,
        capacity_bytes: u64,
    ) {
        self.spool_terminal_removed
            .fetch_add(terminal_removed, Ordering::Relaxed);
        self.spool_orphan_removed
            .fetch_add(orphan_removed, Ordering::Relaxed);
        self.spool_used_bytes.store(used_bytes, Ordering::Relaxed);
        self.spool_capacity_bytes
            .store(capacity_bytes, Ordering::Relaxed);
    }

    pub(crate) fn spool_admitted(&self, bytes: u64) {
        self.spool_used_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn spool_terminal_cleaned(&self, bytes: u64) {
        self.spool_terminal_removed.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .spool_used_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(bytes))
            });
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

    pub(crate) fn qualification_observation_failure(&self) {
        self.qualification_observation_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    fn render(&self) -> String {
        let mut output = String::with_capacity(512);
        self.render_batch(&mut output);
        self.render_spool(&mut output);
        self.render_live(&mut output);
        output
    }

    fn render_batch(&self, output: &mut String) {
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
                "voicetext_batch_admission_rejections_total",
                "Batch requests rejected before audio buffering or provider egress.",
                self.batch_admission_rejections.load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_inflight",
                "Batch provider executions currently tracked.",
                self.batch_inflight.load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_provider_effects_total",
                "Batch provider executions started after durable fencing.",
                self.batch_provider_effects.load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_provider_effects_persisted_total",
                "Batch provider outcomes durably persisted after execution.",
                self.batch_provider_effects_persisted
                    .load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_provider_effects_persistence_unknown_total",
                "Batch provider effects whose post-egress persistence outcome is unknown.",
                self.batch_provider_effects_persistence_unknown
                    .load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_outcome_unknown_total",
                "Batch executions durably classified with unknown provider outcome.",
                self.batch_outcome_unknown.load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_known_terminal_failures_total",
                "Batch executions with a known terminal failure class.",
                self.batch_known_terminal_failures.load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_retryable_outcomes_total",
                "Batch executions durably classified as safe to retry.",
                self.batch_retryable_outcomes.load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_recovery_executed_total",
                "Pre-egress batch jobs resumed during startup recovery.",
                self.batch_recovery_executed.load(Ordering::Relaxed),
            ),
            (
                "voicetext_batch_recovery_unknown_total",
                "Interrupted submissions made terminally unknown during recovery.",
                self.batch_recovery_unknown.load(Ordering::Relaxed),
            ),
        ] {
            write_metric(output, name, help, value);
        }
    }

    fn render_spool(&self, output: &mut String) {
        for (name, help, value) in [
            (
                "voicetext_spool_terminal_removed_total",
                "Terminal batch audio artifacts removed.",
                self.spool_terminal_removed.load(Ordering::Relaxed),
            ),
            (
                "voicetext_spool_orphan_removed_total",
                "Expired orphan batch audio artifacts removed.",
                self.spool_orphan_removed.load(Ordering::Relaxed),
            ),
            (
                "voicetext_spool_used_bytes",
                "Current durable batch audio spool bytes.",
                self.spool_used_bytes.load(Ordering::Relaxed),
            ),
            (
                "voicetext_spool_capacity_bytes",
                "Configured durable batch audio spool capacity.",
                self.spool_capacity_bytes.load(Ordering::Relaxed),
            ),
        ] {
            write_metric(output, name, help, value);
        }
    }

    fn render_live(&self, output: &mut String) {
        for (name, help, value) in [
            (
                "voicetext_qualification_observation_failures_total",
                "Qualification records missing because their opt-in sink failed.",
                self.qualification_observation_failures
                    .load(Ordering::Relaxed),
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
            write_metric(output, name, help, value);
        }
    }
}

fn write_metric(output: &mut String, name: &str, help: &str, value: u64) {
    let _write = writeln!(output, "# HELP {name} {help}");
    let metric_type = if name.ends_with("_bytes") || name.ends_with("_inflight") {
        "gauge"
    } else {
        "counter"
    };
    let _write = writeln!(output, "# TYPE {name} {metric_type}");
    let _write = writeln!(output, "{name} {value}");
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
        metrics.batch_provider_effect_started();
        metrics.batch_provider_effect_persisted();
        metrics.batch_retryable_outcome();
        metrics.live_frame();
        let rendered = metrics.render();
        assert!(rendered.contains("voicetext_batch_requests_total 1\n"));
        assert!(rendered.contains("voicetext_batch_provider_effects_total 1\n"));
        assert!(rendered.contains("voicetext_batch_provider_effects_persisted_total 1\n"));
        assert!(rendered.contains("voicetext_batch_retryable_outcomes_total 1\n"));
        assert!(rendered.contains("voicetext_live_frames_total 1\n"));
        assert!(!rendered.contains('{'));
    }
}
