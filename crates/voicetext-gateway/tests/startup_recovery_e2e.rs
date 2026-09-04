use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;
use voicetext_gateway::contracts::batch::BatchIdentity;
use voicetext_gateway::contracts::batch_projection::GatewayBatchResultProjection;
use voicetext_gateway::profiles::ProfileRegistry;
use voicetext_gateway::secret::MachineSecret;
use voicetext_gateway::server::{
    GatewayLimits, GatewayReadiness, GatewayState, ReadinessFailure, reconcile_startup,
    start_startup_recovery,
};
use voicetext_gateway::storage::{DurableFileSpool, PostgresBatchJobStore};
use voicetext_speech::application::batch::{
    BatchAdmissionOutcome, BatchAdmissionRequest, BatchCoordinator, BatchExecutionOutcome,
};
use voicetext_speech::application::ports::{
    BatchJobId, BatchJobStore, BatchRecognitionRequest, BatchRecognitionResult, BatchRecognizer,
    BatchSegment, BoxFuture, ProviderReference, RecognitionFailure,
};
use voicetext_speech::domain::batch::{
    BatchJobState, BatchProfile, BatchRequestFingerprint, BatchUnknownOutcome,
};

#[derive(Debug, Default)]
struct CountingRecognizer {
    calls: AtomicUsize,
}

impl CountingRecognizer {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl BatchRecognizer for CountingRecognizer {
    fn capabilities(
        &self,
    ) -> &'static voicetext_speech::application::batch_capabilities::BatchCapabilityDescriptor {
        support::batch_capabilities(BatchIdentity::DeepgramNova3MultiV2)
    }

    fn recognize(
        &self,
        request: BatchRecognitionRequest,
    ) -> BoxFuture<'_, Result<BatchRecognitionResult, RecognitionFailure>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = BatchRecognitionResult {
            profile: request.profile,
            text: "recovered synthetic speech".into(),
            duration_millis: request.authoritative_duration_millis,
            provider_duration_millis: Some(request.authoritative_duration_millis),
            segments: vec![BatchSegment {
                start_millis: 0,
                end_millis: request.authoritative_duration_millis,
                text: "recovered synthetic speech".into(),
                confidence: Some(0.99),
                speaker: None,
            }],
            readable_segments: None,
            provider_reference: Some(ProviderReference::new("fake-recovery-request")),
        };
        Box::pin(async move { Ok(result) })
    }
}

#[derive(Debug)]
struct AlwaysReady;

impl GatewayReadiness for AlwaysReady {
    fn check(&self) -> BoxFuture<'_, Result<(), ReadinessFailure>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
#[ignore = "requires VOICETEXT_TEST_DATABASE_URL for a new disposable database"]
async fn durable_startup_recovery_is_exactly_once() {
    let database_url = std::env::var("VOICETEXT_TEST_DATABASE_URL")
        .expect("VOICETEXT_TEST_DATABASE_URL must identify a disposable database");
    let options = PgConnectOptions::from_str(&database_url).expect("valid PostgreSQL URL");
    assert!(
        options
            .get_database()
            .is_some_and(|database| database.starts_with("voicetext_test_")),
        "refusing non-disposable database"
    );
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .unwrap();
    PostgresBatchJobStore::migrate(&pool).await.unwrap();
    let store = Arc::new(PostgresBatchJobStore::new(pool.clone()));
    let spool_root = tempfile::tempdir().unwrap();
    let spool = Arc::new(DurableFileSpool::new(spool_root.path(), 1024).unwrap());
    let recognizer = Arc::new(CountingRecognizer::default());
    let coordinator = BatchCoordinator::new(recognizer.as_ref(), store.as_ref(), spool.as_ref());

    let accepted = admit(&coordinator, 1).await;
    let submitting = admit(&coordinator, 2).await;
    let completed = admit(&coordinator, 3).await;
    let mut interrupted = store.load(&submitting).await.unwrap().unwrap();
    interrupted.job.begin_submission().unwrap();
    store
        .compare_and_swap(interrupted.revision, interrupted)
        .await
        .unwrap();
    assert!(matches!(
        coordinator
            .execute(&completed, &GatewayBatchResultProjection)
            .await
            .unwrap(),
        BatchExecutionOutcome::Persisted(_)
    ));
    assert_eq!(recognizer.calls(), 1);

    let profiles = ProfileRegistry::new().with_batch(recognizer.clone());
    let state = GatewayState::new(
        MachineSecret::from_token(b"recovery-e2e-token-32-byte-fixture").unwrap(),
        store.clone(),
        spool,
        profiles,
        Arc::new(AlwaysReady),
        GatewayLimits::new(
            1024,
            1024,
            NonZeroUsize::new(2).unwrap(),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap(),
    );
    let recovery = reconcile_startup(&state).await.unwrap();

    assert_eq!(recovery.summary.recovered_unknown, 1);
    assert_eq!(recognizer.calls(), 1);
    start_startup_recovery(&state, recovery);
    wait_for_state(store.as_ref(), &accepted, |state| {
        matches!(state, BatchJobState::Completed { .. })
    })
    .await;
    assert_eq!(recognizer.calls(), 2);
    assert!(matches!(
        store.load(&accepted).await.unwrap().unwrap().job.state(),
        BatchJobState::Completed { .. }
    ));
    assert!(matches!(
        store.load(&completed).await.unwrap().unwrap().job.state(),
        BatchJobState::Completed { .. }
    ));
    assert!(matches!(
        store.load(&submitting).await.unwrap().unwrap().job.state(),
        BatchJobState::OutcomeUnknown {
            reason: BatchUnknownOutcome::InterruptedSubmission,
            ..
        }
    ));

    let replay = reconcile_startup(&state).await.unwrap();
    assert_eq!(replay.summary.recovered_unknown, 0);
    start_startup_recovery(&state, replay);
    tokio::task::yield_now().await;
    assert_eq!(recognizer.calls(), 2);
    state
        .shutdown_batch_tasks(tokio::time::Instant::now() + Duration::from_secs(5))
        .await;
    pool.close().await;
}

async fn wait_for_state(
    store: &PostgresBatchJobStore,
    id: &BatchJobId,
    expected: impl Fn(&BatchJobState) -> bool,
) {
    for _ in 0..100 {
        let snapshot = store.load(id).await.unwrap().unwrap();
        if expected(snapshot.job.state()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("recovery worker did not persist the expected state");
}

async fn admit(coordinator: &BatchCoordinator<'_>, identity: u8) -> BatchJobId {
    let id = BatchJobId::new(Uuid::new_v4().hyphenated().to_string());
    let outcome = coordinator
        .admit(BatchAdmissionRequest {
            id: id.clone(),
            profile: BatchProfile::new(2, "deepgram", "nova-3", "multi").unwrap(),
            fingerprint: BatchRequestFingerprint::from_bytes([identity; 32]),
            audio: vec![identity; 32],
            authoritative_duration_millis: 100,
            keyterms: vec![format!("fixture-{identity}")],
        })
        .await
        .unwrap();
    assert!(matches!(outcome, BatchAdmissionOutcome::Accepted(_)));
    id
}
#[allow(dead_code, unused_imports)]
mod support;
