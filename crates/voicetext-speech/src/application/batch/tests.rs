use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use super::*;
use crate::application::ports::{
    BatchAudioSpoolFailure, BatchAudioStoreOutcome, BatchRecognitionResult,
    BatchResultProjectionFailure, BatchSegment, RecognitionFailure,
};
use crate::domain::batch::{BatchProfile, BatchRequestFingerprint};

struct Fake {
    jobs: Mutex<Vec<BatchJobSnapshot>>,
    audio: Mutex<Option<Vec<u8>>>,
    calls: AtomicUsize,
    cas_calls: AtomicUsize,
    conflict_at: AtomicUsize,
    fail_cas_at: AtomicUsize,
    removes: AtomicUsize,
    fail_insert: AtomicBool,
    fail_spool: AtomicBool,
    fail_remove: AtomicBool,
}

impl Fake {
    fn new() -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            audio: Mutex::new(None),
            calls: AtomicUsize::new(0),
            cas_calls: AtomicUsize::new(0),
            conflict_at: AtomicUsize::new(0),
            fail_cas_at: AtomicUsize::new(0),
            removes: AtomicUsize::new(0),
            fail_insert: AtomicBool::new(false),
            fail_spool: AtomicBool::new(false),
            fail_remove: AtomicBool::new(false),
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

impl BatchResultProjection for Fake {
    fn validate(
        &self,
        _id: &BatchJobId,
        _result: &BatchRecognitionResult,
    ) -> Result<(), BatchResultProjectionFailure> {
        Ok(())
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
        if self.fail_remove.load(Ordering::SeqCst) {
            return Box::pin(async {
                Err(BatchAudioSpoolFailure::Unavailable {
                    code: "REMOVE_DOWN".into(),
                })
            });
        }
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
        if self.fail_cas_at.load(Ordering::SeqCst) == call {
            return Box::pin(async { Err(unavailable()) });
        }
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

    fn recovery_head(&self) -> BoxFuture<'_, Result<Option<BatchJobId>, BatchJobStoreFailure>> {
        let head = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .max_by_key(|row| row.id.as_str())
            .map(|row| row.id.clone());
        Box::pin(async move { Ok(head) })
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
    let loser = run(coordinator.execute(&BatchJobId::new("job"), &fake)).unwrap();
    assert!(matches!(loser, BatchExecutionOutcome::NotClaimed(_)));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    let winner = run(coordinator.execute(&BatchJobId::new("job"), &fake)).unwrap();
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
        run(coordinator.execute(&BatchJobId::new("job"), &fake)),
        Err(BatchCoordinatorFailure::PostEgressConflict)
    );
    assert!(fake.audio.lock().unwrap().is_some());
    assert_eq!(fake.removes.load(Ordering::SeqCst), 0);

    let report = run(coordinator.recover_startup_through(None, BatchJobId::new("job"))).unwrap();
    assert_eq!(report.recovered_unknown.len(), 1);
    assert!(fake.audio.lock().unwrap().is_none());
    assert_eq!(fake.removes.load(Ordering::SeqCst), 1);
}

#[test]
fn paid_result_persistence_failure_is_post_egress_and_preserves_audio() {
    let fake = Fake::new();
    let coordinator = BatchCoordinator::new(&fake, &fake, &fake);
    run(coordinator.admit(request(1))).unwrap();
    fake.fail_cas_at.store(2, Ordering::SeqCst);

    assert!(matches!(
        run(coordinator.execute(&BatchJobId::new("job"), &fake)),
        Err(BatchCoordinatorFailure::PostEgressStore(_))
    ));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert!(fake.audio.lock().unwrap().is_some());
    assert_eq!(fake.removes.load(Ordering::SeqCst), 0);
}

#[test]
fn pre_persistence_spool_read_failure_remains_a_spool_error_without_egress() {
    let fake = Fake::new();
    let coordinator = BatchCoordinator::new(&fake, &fake, &fake);
    run(coordinator.admit(request(1))).unwrap();
    *fake.audio.lock().unwrap() = None;

    assert!(matches!(
        run(coordinator.execute_with_cleanup_report(&BatchJobId::new("job"), &fake)),
        Err(BatchCoordinatorFailure::Spool(
            BatchAudioSpoolFailure::Missing
        ))
    ));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        fake.jobs.lock().unwrap()[0].job.state(),
        BatchJobState::Accepted
    ));
}

#[test]
fn terminal_cleanup_failure_preserves_the_stored_result_and_replays_without_egress() {
    let fake = Fake::new();
    let coordinator = BatchCoordinator::new(&fake, &fake, &fake);
    run(coordinator.admit(request(1))).unwrap();
    fake.fail_remove.store(true, Ordering::SeqCst);

    let report =
        run(coordinator.execute_with_cleanup_report(&BatchJobId::new("job"), &fake)).unwrap();
    let BatchExecutionOutcome::Persisted(stored) = report.outcome else {
        panic!("terminal outcome was not stored")
    };
    assert!(stored.job.state().is_terminal());
    assert!(report.post_persistence_cleanup_failure.is_some());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert!(fake.audio.lock().unwrap().is_some());

    let replay = run(coordinator.admit(request(1))).unwrap();
    assert!(matches!(replay, BatchAdmissionOutcome::Replay(snapshot) if snapshot == stored));
    let not_actionable = run(coordinator.execute(&BatchJobId::new("job"), &fake)).unwrap();
    assert!(
        matches!(not_actionable, BatchExecutionOutcome::NotActionable(snapshot) if snapshot == stored)
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
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
    let head = BatchJobId::new(format!("job-{:03}", RECOVERY_BATCH_LIMIT.get()));
    let report =
        run(BatchCoordinator::new(&fake, &fake, &fake).recover_startup_through(None, head))
            .unwrap();
    assert_eq!(report.actionable.len(), RECOVERY_BATCH_LIMIT.get());
    assert!(report.next_cursor.is_some());
    let tail = run(BatchCoordinator::new(&fake, &fake, &fake)
        .recover_startup_through(report.next_cursor, BatchJobId::new("job-100")))
    .unwrap();
    assert_eq!(tail.actionable.len(), 1);
    let frozen = run(BatchCoordinator::new(&fake, &fake, &fake)
        .recover_startup_through(None, BatchJobId::new("job-050")))
    .unwrap();
    assert_eq!(frozen.actionable.len(), 51);
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
