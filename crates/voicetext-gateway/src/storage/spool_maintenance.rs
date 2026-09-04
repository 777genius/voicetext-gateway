//! Crash-safe bounded maintenance for terminal and orphaned spool artifacts.

use std::fs;
use std::time::{Duration, SystemTime};

use voicetext_speech::application::ports::{
    BatchAudioHandle, BatchAudioRemoveOutcome, BatchAudioSpool, BatchAudioSpoolFailure, BatchJobId,
    BatchJobStore,
};

use super::spool::{DurableFileSpool, parse_handle, spool_audio_bytes};

/// Fixed-cardinality evidence from one startup maintenance pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpoolMaintenanceReport {
    pub terminal_removed: u64,
    pub orphan_removed: u64,
    pub temporary_removed: u64,
    pub preserved: u64,
    pub used_bytes: u64,
    pub capacity_bytes: u64,
}

impl DurableFileSpool {
    /// Removes terminal audio and expired crash orphans while preserving live ledger artifacts.
    ///
    /// # Errors
    ///
    /// Fails closed on unreadable storage or ledger state. Orphans younger than `retention` remain.
    #[expect(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "temporary spool artifacts use an exact lowercase suffix"
    )]
    pub async fn reconcile(
        &self,
        jobs: &dyn BatchJobStore,
        retention: Duration,
    ) -> Result<SpoolMaintenanceReport, BatchAudioSpoolFailure> {
        let now = SystemTime::now();
        let mut report = SpoolMaintenanceReport {
            capacity_bytes: self.maximum_total_bytes,
            ..SpoolMaintenanceReport::default()
        };
        let entries = fs::read_dir(&self.root)
            .map_err(io_failure)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(io_failure)?;
        for entry in entries {
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_failure(error)),
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') && name.ends_with(".tmp") {
                if expired(&metadata, now, retention) {
                    fs::remove_file(entry.path()).map_err(io_failure)?;
                    report.temporary_removed = report.temporary_removed.saturating_add(1);
                }
                continue;
            }
            if let Some(job_id) = name.strip_suffix(".identity") {
                if !matching_audio_exists(&self.root, job_id)? && expired(&metadata, now, retention)
                {
                    fs::remove_file(entry.path()).map_err(io_failure)?;
                    report.orphan_removed = report.orphan_removed.saturating_add(1);
                }
                continue;
            }
            let Ok(parsed) = parse_handle(name) else {
                continue;
            };
            let handle = BatchAudioHandle::new(name);
            let id = BatchJobId::new(parsed.job_id);
            let snapshot = jobs.load(&id).await.map_err(|_| ledger_failure())?;
            match snapshot {
                Some(snapshot) if snapshot.audio != handle => return Err(ledger_failure()),
                Some(snapshot) if snapshot.job.state().is_terminal() => {
                    if self.remove(&handle).await? == BatchAudioRemoveOutcome::Removed {
                        report.terminal_removed = report.terminal_removed.saturating_add(1);
                    }
                }
                None if expired(&metadata, now, retention) => {
                    if self.remove(&handle).await? == BatchAudioRemoveOutcome::Removed {
                        report.orphan_removed = report.orphan_removed.saturating_add(1);
                    }
                }
                Some(_) | None => report.preserved = report.preserved.saturating_add(1),
            }
        }
        report.used_bytes = spool_audio_bytes(&self.root)?;
        Ok(report)
    }
}

fn matching_audio_exists(
    root: &std::path::Path,
    job_id: &str,
) -> Result<bool, BatchAudioSpoolFailure> {
    for entry in fs::read_dir(root).map_err(io_failure)? {
        let entry = entry.map_err(io_failure)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if parse_handle(name).is_ok_and(|parsed| parsed.job_id == job_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn expired(metadata: &fs::Metadata, now: SystemTime, retention: Duration) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= retention)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "I/O Result::map_err transfers ownership"
)]
fn io_failure(error: std::io::Error) -> BatchAudioSpoolFailure {
    BatchAudioSpoolFailure::Unavailable {
        code: format!("SPOOL_MAINTENANCE_{:?}", error.kind()).to_ascii_uppercase(),
    }
}

fn ledger_failure() -> BatchAudioSpoolFailure {
    BatchAudioSpoolFailure::Unavailable {
        code: "SPOOL_MAINTENANCE_LEDGER_UNAVAILABLE".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Mutex;

    use tempfile::tempdir;
    use voicetext_speech::application::ports::{
        BatchAudioStoreOutcome, BatchJobInsertOutcome, BatchJobSnapshot, BatchJobStoreFailure,
        BatchJobUpdateOutcome, BoxFuture,
    };
    use voicetext_speech::domain::batch::{
        BatchFailure, BatchJob, BatchProfile, BatchRequestFingerprint,
    };

    use super::*;

    struct Ledger(Mutex<Vec<BatchJobSnapshot>>);

    impl BatchJobStore for Ledger {
        fn load<'a>(
            &'a self,
            id: &'a BatchJobId,
        ) -> BoxFuture<'a, Result<Option<BatchJobSnapshot>, BatchJobStoreFailure>> {
            let result = self
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|row| row.id == *id)
                .cloned();
            Box::pin(async move { Ok(result) })
        }

        fn insert(
            &self,
            _: BatchJobId,
            _: BatchJob,
            _: BatchAudioHandle,
            _: u64,
            _: Vec<String>,
        ) -> BoxFuture<'_, Result<BatchJobInsertOutcome, BatchJobStoreFailure>> {
            Box::pin(async { panic!("maintenance does not insert") })
        }

        fn compare_and_swap(
            &self,
            _: u64,
            _: BatchJobSnapshot,
        ) -> BoxFuture<'_, Result<BatchJobUpdateOutcome, BatchJobStoreFailure>> {
            Box::pin(async { panic!("maintenance does not update") })
        }

        fn recovery_head(&self) -> BoxFuture<'_, Result<Option<BatchJobId>, BatchJobStoreFailure>> {
            Box::pin(async { panic!("maintenance does not inspect recovery head") })
        }

        fn list_recovery_candidates(
            &self,
            _: Option<BatchJobId>,
            _: NonZeroUsize,
        ) -> BoxFuture<'_, Result<Vec<BatchJobSnapshot>, BatchJobStoreFailure>> {
            Box::pin(async { panic!("maintenance does not list recovery candidates") })
        }
    }

    #[tokio::test]
    async fn preserves_accepted_and_inflight_but_removes_terminal_and_orphan_audio() {
        let directory = tempdir().unwrap();
        let spool = DurableFileSpool::new(directory.path(), 16).unwrap();
        let accepted = id("123e4567-e89b-12d3-a456-426614174000");
        let submitting = id("223e4567-e89b-12d3-a456-426614174000");
        let terminal = id("323e4567-e89b-12d3-a456-426614174000");
        let orphan = id("423e4567-e89b-12d3-a456-426614174000");
        let accepted_audio = store(&spool, accepted.clone(), b"accepted").await;
        let submitting_audio = store(&spool, submitting.clone(), b"submitting").await;
        let terminal_audio = store(&spool, terminal.clone(), b"terminal").await;
        let orphan_audio = store(&spool, orphan, b"orphan").await;

        let mut submitting_job = job();
        submitting_job.begin_submission().unwrap();
        let mut terminal_job = job();
        terminal_job.begin_submission().unwrap();
        terminal_job
            .fail(BatchFailure::new("TERMINAL").unwrap())
            .unwrap();
        let ledger = Ledger(Mutex::new(vec![
            snapshot(accepted, job(), accepted_audio.clone()),
            snapshot(submitting, submitting_job, submitting_audio.clone()),
            snapshot(terminal, terminal_job, terminal_audio.clone()),
        ]));

        let report = spool.reconcile(&ledger, Duration::ZERO).await.unwrap();
        assert_eq!(report.preserved, 2);
        assert_eq!(report.terminal_removed, 1);
        assert_eq!(report.orphan_removed, 1);
        assert_eq!(spool.read(&accepted_audio).await.unwrap(), b"accepted");
        assert_eq!(spool.read(&submitting_audio).await.unwrap(), b"submitting");
        assert_eq!(
            spool.read(&terminal_audio).await,
            Err(BatchAudioSpoolFailure::Missing)
        );
        assert_eq!(
            spool.read(&orphan_audio).await,
            Err(BatchAudioSpoolFailure::Missing)
        );
    }

    fn id(value: &str) -> BatchJobId {
        BatchJobId::new(value)
    }

    fn job() -> BatchJob {
        BatchJob::accept(
            BatchProfile::new(2, "provider", "model", "multi").unwrap(),
            BatchRequestFingerprint::from_bytes([1; 32]),
        )
    }

    async fn store(spool: &DurableFileSpool, id: BatchJobId, audio: &[u8]) -> BatchAudioHandle {
        let BatchAudioStoreOutcome::Stored(handle) = spool.store(id, audio.to_vec()).await.unwrap()
        else {
            panic!("new test artifact must be owned")
        };
        handle
    }

    fn snapshot(id: BatchJobId, job: BatchJob, audio: BatchAudioHandle) -> BatchJobSnapshot {
        BatchJobSnapshot {
            id,
            job,
            audio,
            authoritative_duration_millis: 1,
            keyterms: Vec::new(),
            provider_reference: None,
            retry_after_millis: None,
            result: None,
            revision: 0,
        }
    }
}
