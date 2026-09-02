//! Runtime-independent orchestration for durable batch recognition.

use std::fmt;
use std::num::NonZeroUsize;

use crate::application::batch_models::apply_recognition_outcome;
pub use crate::application::batch_models::{
    BatchAdmissionOutcome, BatchAdmissionRequest, BatchCoordinatorFailure, BatchExecutionOutcome,
    BatchStartupRecovery,
};
use crate::application::ports::{
    BatchAudioHandle, BatchAudioSpool, BatchAudioStoreOutcome, BatchJobId, BatchJobInsertOutcome,
    BatchJobSnapshot, BatchJobStore, BatchJobStoreFailure, BatchJobUpdateOutcome,
    BatchRecognitionRequest, BatchRecognizer, BoxFuture,
};
use crate::domain::batch::{BatchJob, BatchJobState};

const RECOVERY_BATCH_LIMIT: NonZeroUsize = NonZeroUsize::new(100).unwrap();

/// Coordinates admission, a single fenced provider call, and startup recovery.
pub struct BatchCoordinator<'a> {
    recognizer: &'a dyn BatchRecognizer,
    jobs: &'a dyn BatchJobStore,
    spool: &'a dyn BatchAudioSpool,
}

impl fmt::Debug for BatchCoordinator<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BatchCoordinator")
            .finish_non_exhaustive()
    }
}

impl<'a> BatchCoordinator<'a> {
    pub const fn new(
        recognizer: &'a dyn BatchRecognizer,
        jobs: &'a dyn BatchJobStore,
        spool: &'a dyn BatchAudioSpool,
    ) -> Self {
        Self {
            recognizer,
            jobs,
            spool,
        }
    }

    pub fn admit(
        &self,
        mut request: BatchAdmissionRequest,
    ) -> BoxFuture<'_, Result<BatchAdmissionOutcome, BatchCoordinatorFailure>> {
        Box::pin(async move {
            if let Some(existing) = self.jobs.load(&request.id).await.map_err(store)? {
                return classify_existing(existing, &request);
            }
            let stored = self
                .spool
                .store(request.id.clone(), std::mem::take(&mut request.audio))
                .await
                .map_err(BatchCoordinatorFailure::Spool)?;
            let (audio, owns_audio) = match stored {
                BatchAudioStoreOutcome::Stored(handle) => (handle, true),
                BatchAudioStoreOutcome::Existing(handle) => (handle, false),
            };
            let insert = self
                .jobs
                .insert(
                    request.id.clone(),
                    BatchJob::accept(request.profile.clone(), request.fingerprint),
                    audio.clone(),
                    request.authoritative_duration_millis,
                    request.keyterms.clone(),
                )
                .await;
            match insert {
                Ok(BatchJobInsertOutcome::Inserted(snapshot)) => {
                    Ok(BatchAdmissionOutcome::Accepted(snapshot))
                }
                Ok(BatchJobInsertOutcome::Existing(existing)) => {
                    self.resolve_race(existing, &request, owns_audio, &audio)
                        .await
                }
                Err(insert) => match self.jobs.load(&request.id).await {
                    Ok(Some(existing)) => {
                        self.resolve_race(existing, &request, owns_audio, &audio)
                            .await
                    }
                    Ok(None) if owns_audio => {
                        self.spool.remove(&audio).await.map_err(|spool| {
                            BatchCoordinatorFailure::AdmissionCleanup {
                                store: Some(insert.clone()),
                                spool,
                            }
                        })?;
                        Err(BatchCoordinatorFailure::Store(insert))
                    }
                    Ok(None) => Err(BatchCoordinatorFailure::Store(insert)),
                    Err(verification) => Err(BatchCoordinatorFailure::AdmissionStoreUncertain {
                        insert,
                        verification,
                    }),
                },
            }
        })
    }

    async fn resolve_race(
        &self,
        existing: BatchJobSnapshot,
        request: &BatchAdmissionRequest,
        owns_audio: bool,
        candidate: &BatchAudioHandle,
    ) -> Result<BatchAdmissionOutcome, BatchCoordinatorFailure> {
        if owns_audio && candidate != &existing.audio {
            self.spool.remove(candidate).await.map_err(|spool| {
                BatchCoordinatorFailure::AdmissionCleanup { store: None, spool }
            })?;
        }
        classify_existing(existing, request)
    }

    /// Claims an actionable state with CAS before invoking recognition exactly once.
    pub fn execute(
        &self,
        id: &BatchJobId,
    ) -> BoxFuture<'_, Result<BatchExecutionOutcome, BatchCoordinatorFailure>> {
        let id = id.clone();
        Box::pin(async move {
            let Some(snapshot) = self.jobs.load(&id).await.map_err(store)? else {
                return Ok(BatchExecutionOutcome::NotClaimed(None));
            };
            if !matches!(
                snapshot.job.state(),
                BatchJobState::Accepted | BatchJobState::Retryable { .. }
            ) {
                return Ok(BatchExecutionOutcome::NotActionable(snapshot));
            }
            let audio = self
                .spool
                .read(&snapshot.audio)
                .await
                .map_err(BatchCoordinatorFailure::Spool)?;
            let expected_revision = snapshot.revision;
            let mut claimed = snapshot;
            claimed
                .job
                .begin_submission()
                .map_err(BatchCoordinatorFailure::Transition)?;
            claimed.provider_reference = None;
            claimed.retry_after_millis = None;
            claimed.result = None;
            let claimed = match self
                .jobs
                .compare_and_swap(expected_revision, claimed)
                .await
                .map_err(store)?
            {
                BatchJobUpdateOutcome::Stored(snapshot) => snapshot,
                BatchJobUpdateOutcome::RevisionConflict(snapshot) => {
                    return Ok(BatchExecutionOutcome::NotClaimed(Some(snapshot)));
                }
                BatchJobUpdateOutcome::Missing => {
                    return Ok(BatchExecutionOutcome::NotClaimed(None));
                }
            };
            let recognition = self
                .recognizer
                .recognize(BatchRecognitionRequest {
                    profile: claimed.job.profile().clone(),
                    audio,
                    authoritative_duration_millis: claimed.authoritative_duration_millis,
                    keyterms: claimed.keyterms.clone(),
                })
                .await;
            let replacement = apply_recognition_outcome(claimed.clone(), recognition)
                .map_err(BatchCoordinatorFailure::Transition)?;
            match self
                .jobs
                .compare_and_swap(claimed.revision, replacement)
                .await
            {
                Ok(BatchJobUpdateOutcome::Stored(snapshot)) => {
                    if snapshot.job.state().is_terminal() {
                        self.spool
                            .remove(&snapshot.audio)
                            .await
                            .map_err(BatchCoordinatorFailure::Spool)?;
                    }
                    Ok(BatchExecutionOutcome::Persisted(snapshot))
                }
                Ok(BatchJobUpdateOutcome::RevisionConflict(_) | BatchJobUpdateOutcome::Missing) => {
                    Err(BatchCoordinatorFailure::PostEgressConflict)
                }
                Err(failure) => Err(BatchCoordinatorFailure::PostEgressStore(failure)),
            }
        })
    }

    /// Makes interrupted submissions terminally unknown and returns safe pending work.
    pub fn recover_startup(
        &self,
        after: Option<BatchJobId>,
    ) -> BoxFuture<'_, Result<BatchStartupRecovery, BatchCoordinatorFailure>> {
        Box::pin(async move {
            let mut candidates = self
                .jobs
                .list_recovery_candidates(after, RECOVERY_BATCH_LIMIT)
                .await
                .map_err(store)?;
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
                            BatchJobUpdateOutcome::Missing => {
                                report.missing.push(id);
                            }
                        }
                    }
                    _ => report.invalid_candidates.push(snapshot),
                }
            }
            Ok(report)
        })
    }
}

fn classify_existing(
    existing: BatchJobSnapshot,
    request: &BatchAdmissionRequest,
) -> Result<BatchAdmissionOutcome, BatchCoordinatorFailure> {
    if existing.job.profile() == &request.profile
        && existing.job.fingerprint() == request.fingerprint
    {
        Ok(BatchAdmissionOutcome::Replay(existing))
    } else {
        Err(BatchCoordinatorFailure::AdmissionConflict(Box::new(
            existing,
        )))
    }
}

fn store(failure: BatchJobStoreFailure) -> BatchCoordinatorFailure {
    BatchCoordinatorFailure::Store(failure)
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::application::ports::{
        BatchAudioSpoolFailure, BatchAudioStoreOutcome, BatchRecognitionResult, BatchSegment,
        RecognitionFailure,
    };
    use crate::domain::batch::{BatchProfile, BatchRequestFingerprint};

    struct Fake {
        jobs: Mutex<Vec<BatchJobSnapshot>>,
        audio: Mutex<Option<Vec<u8>>>,
        calls: AtomicUsize,
        cas_calls: AtomicUsize,
        conflict_at: AtomicUsize,
        removes: AtomicUsize,
        fail_insert: AtomicBool,
        fail_spool: AtomicBool,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                jobs: Mutex::new(Vec::new()),
                audio: Mutex::new(None),
                calls: AtomicUsize::new(0),
                cas_calls: AtomicUsize::new(0),
                conflict_at: AtomicUsize::new(0),
                removes: AtomicUsize::new(0),
                fail_insert: AtomicBool::new(false),
                fail_spool: AtomicBool::new(false),
            }
        }
    }

    impl BatchRecognizer for Fake {
        fn capabilities(
            &self,
        ) -> &'static crate::application::batch_capabilities::BatchCapabilityDescriptor {
            panic!("application fake has no composition capabilities")
        }

        fn recognize(
            &self,
            _request: BatchRecognitionRequest,
        ) -> BoxFuture<'_, Result<BatchRecognitionResult, RecognitionFailure>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(result()) })
        }
    }

    impl BatchAudioSpool for Fake {
        fn store(
            &self,
            _id: BatchJobId,
            bytes: Vec<u8>,
        ) -> BoxFuture<'_, Result<BatchAudioStoreOutcome, BatchAudioSpoolFailure>> {
            let outcome = if self.fail_spool.load(Ordering::SeqCst) {
                Err(BatchAudioSpoolFailure::Unavailable {
                    code: "DOWN".into(),
                })
            } else {
                let mut audio = self.audio.lock().unwrap();
                match audio.as_ref() {
                    Some(existing) if existing == &bytes => Ok(BatchAudioStoreOutcome::Existing(
                        BatchAudioHandle::new("audio"),
                    )),
                    Some(_) => Err(BatchAudioSpoolFailure::IdentityConflict),
                    None => {
                        *audio = Some(bytes);
                        Ok(BatchAudioStoreOutcome::Stored(BatchAudioHandle::new(
                            "audio",
                        )))
                    }
                }
            };
            Box::pin(async move { outcome })
        }

        fn read<'b>(
            &'b self,
            _handle: &'b BatchAudioHandle,
        ) -> BoxFuture<'b, Result<Vec<u8>, BatchAudioSpoolFailure>> {
            let audio = self.audio.lock().unwrap();
            let result = audio.clone().ok_or(BatchAudioSpoolFailure::Missing);
            Box::pin(async move { result })
        }

        fn remove<'b>(
            &'b self,
            _handle: &'b BatchAudioHandle,
        ) -> BoxFuture<'b, Result<(), BatchAudioSpoolFailure>> {
            self.removes.fetch_add(1, Ordering::SeqCst);
            *self.audio.lock().unwrap() = None;
            Box::pin(async { Ok(()) })
        }
    }

    impl BatchJobStore for Fake {
        fn load<'b>(
            &'b self,
            id: &'b BatchJobId,
        ) -> BoxFuture<'b, Result<Option<BatchJobSnapshot>, BatchJobStoreFailure>> {
            let jobs = self.jobs.lock().unwrap();
            let found = jobs.iter().find(|row| row.id == *id).cloned();
            Box::pin(async move { Ok(found) })
        }

        fn insert(
            &self,
            id: BatchJobId,
            job: BatchJob,
            audio: BatchAudioHandle,
            authoritative_duration_millis: u64,
            keyterms: Vec<String>,
        ) -> BoxFuture<'_, Result<BatchJobInsertOutcome, BatchJobStoreFailure>> {
            if self.fail_insert.load(Ordering::SeqCst) {
                return Box::pin(async { Err(unavailable()) });
            }
            let mut jobs = self.jobs.lock().unwrap();
            let outcome = if let Some(row) = jobs.iter().find(|row| row.id == id) {
                BatchJobInsertOutcome::Existing(row.clone())
            } else {
                let row = BatchJobSnapshot {
                    id,
                    job,
                    audio,
                    authoritative_duration_millis,
                    keyterms,
                    provider_reference: None,
                    retry_after_millis: None,
                    result: None,
                    revision: 0,
                };
                jobs.push(row.clone());
                BatchJobInsertOutcome::Inserted(row)
            };
            Box::pin(async move { Ok(outcome) })
        }

        fn compare_and_swap(
            &self,
            expected_revision: u64,
            mut replacement: BatchJobSnapshot,
        ) -> BoxFuture<'_, Result<BatchJobUpdateOutcome, BatchJobStoreFailure>> {
            let call = self.cas_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let mut jobs = self.jobs.lock().unwrap();
            let outcome = match jobs.iter_mut().find(|row| row.id == replacement.id) {
                None => BatchJobUpdateOutcome::Missing,
                Some(row)
                    if row.revision != expected_revision
                        || self.conflict_at.load(Ordering::SeqCst) == call =>
                {
                    BatchJobUpdateOutcome::RevisionConflict(row.clone())
                }
                Some(row) => {
                    replacement.revision = expected_revision + 1;
                    row.clone_from(&replacement);
                    BatchJobUpdateOutcome::Stored(replacement)
                }
            };
            Box::pin(async move { Ok(outcome) })
        }

        fn list_recovery_candidates(
            &self,
            after: Option<BatchJobId>,
            maximum: NonZeroUsize,
        ) -> BoxFuture<'_, Result<Vec<BatchJobSnapshot>, BatchJobStoreFailure>> {
            assert_eq!(maximum, RECOVERY_BATCH_LIMIT);
            let mut rows = self.jobs.lock().unwrap().clone();
            if let Some(after) = after {
                rows.retain(|row| row.id.as_str() > after.as_str());
            }
            rows.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
            rows.truncate(maximum.get());
            Box::pin(async move { Ok(rows) })
        }
    }

    fn profile() -> BatchProfile {
        BatchProfile::new(2, "provider", "model", "multi").unwrap()
    }

    fn request(byte: u8) -> BatchAdmissionRequest {
        BatchAdmissionRequest {
            id: BatchJobId::new("job"),
            profile: profile(),
            fingerprint: BatchRequestFingerprint::from_bytes([byte; 32]),
            audio: vec![1, 2],
            authoritative_duration_millis: 100,
            keyterms: vec!["term".into()],
        }
    }

    fn result() -> BatchRecognitionResult {
        BatchRecognitionResult {
            profile: profile(),
            text: "hello".into(),
            duration_millis: 100,
            provider_duration_millis: Some(99),
            segments: vec![BatchSegment {
                start_millis: 0,
                end_millis: 100,
                text: "hello".into(),
                confidence: Some(0.9),
                speaker: None,
            }],
            readable_segments: None,
            provider_reference: None,
        }
    }

    fn unavailable() -> BatchJobStoreFailure {
        BatchJobStoreFailure::Unavailable {
            code: "DOWN".into(),
        }
    }

    fn run<T>(future: impl Future<Output = T>) -> T {
        let waker = Waker::noop();
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut Context::from_waker(waker)) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("fake future unexpectedly pending"),
        }
    }

    #[test]
    fn admission_replays_exact_identity_and_rejects_conflict_without_cleanup() {
        let fake = Fake::new();
        let coordinator = BatchCoordinator::new(&fake, &fake, &fake);
        let accepted = run(coordinator.admit(request(1)));
        assert!(matches!(accepted, Ok(BatchAdmissionOutcome::Accepted(_))));
        let replay = run(coordinator.admit(request(1)));
        assert!(matches!(replay, Ok(BatchAdmissionOutcome::Replay(_))));
        let conflict = run(coordinator.admit(request(2)));
        assert!(matches!(
            conflict,
            Err(BatchCoordinatorFailure::AdmissionConflict(_))
        ));
        assert_eq!(fake.removes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn only_the_cas_winner_calls_the_provider() {
        let fake = Fake::new();
        let coordinator = BatchCoordinator::new(&fake, &fake, &fake);
        run(coordinator.admit(request(1))).unwrap();
        fake.conflict_at.store(1, Ordering::SeqCst);
        let loser = run(coordinator.execute(&BatchJobId::new("job"))).unwrap();
        assert!(matches!(loser, BatchExecutionOutcome::NotClaimed(_)));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        let winner = run(coordinator.execute(&BatchJobId::new("job"))).unwrap();
        assert!(matches!(winner, BatchExecutionOutcome::Persisted(_)));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.removes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn audio_survives_until_a_terminal_cas_is_durable() {
        let fake = Fake::new();
        let coordinator = BatchCoordinator::new(&fake, &fake, &fake);
        run(coordinator.admit(request(1))).unwrap();
        assert!(fake.audio.lock().unwrap().is_some());

        fake.conflict_at.store(2, Ordering::SeqCst);
        assert_eq!(
            run(coordinator.execute(&BatchJobId::new("job"))),
            Err(BatchCoordinatorFailure::PostEgressConflict)
        );
        assert!(fake.audio.lock().unwrap().is_some());
        assert_eq!(fake.removes.load(Ordering::SeqCst), 0);

        let report = run(coordinator.recover_startup(None)).unwrap();
        assert_eq!(report.recovered_unknown.len(), 1);
        assert!(fake.audio.lock().unwrap().is_none());
        assert_eq!(fake.removes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn startup_recovery_page_is_bounded_and_returns_a_cursor() {
        let fake = Fake::new();
        for index in 0..=RECOVERY_BATCH_LIMIT.get() {
            fake.jobs
                .lock()
                .unwrap()
                .push(accepted_snapshot(&format!("job-{index:03}")));
        }
        let report = run(BatchCoordinator::new(&fake, &fake, &fake).recover_startup(None)).unwrap();
        assert_eq!(report.actionable.len(), RECOVERY_BATCH_LIMIT.get());
        assert!(report.next_cursor.is_some());
    }

    #[test]
    fn spool_and_store_failures_do_not_reach_provider() {
        let fake = Fake::new();
        let coordinator = BatchCoordinator::new(&fake, &fake, &fake);
        fake.fail_spool.store(true, Ordering::SeqCst);
        let spool = run(coordinator.admit(request(1)));
        assert!(matches!(spool, Err(BatchCoordinatorFailure::Spool(_))));
        fake.fail_spool.store(false, Ordering::SeqCst);
        fake.fail_insert.store(true, Ordering::SeqCst);
        let store = run(coordinator.admit(request(1)));
        assert!(matches!(store, Err(BatchCoordinatorFailure::Store(_))));
        assert_eq!(fake.removes.load(Ordering::SeqCst), 1);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    fn accepted_snapshot(id: &str) -> BatchJobSnapshot {
        BatchJobSnapshot {
            id: BatchJobId::new(id),
            job: BatchJob::accept(profile(), BatchRequestFingerprint::from_bytes([1; 32])),
            audio: BatchAudioHandle::new("audio"),
            authoritative_duration_millis: 100,
            keyterms: Vec::new(),
            provider_reference: None,
            retry_after_millis: None,
            result: None,
            revision: 0,
        }
    }
}
