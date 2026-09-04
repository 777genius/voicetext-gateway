#[allow(dead_code, unused_imports)]
mod support;

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use tokio::net::TcpListener;
use uuid::Uuid;
use voicetext_gateway::contracts::batch::BatchIdentity;
use voicetext_gateway::profiles::ProfileRegistry;
use voicetext_gateway::secret::MachineSecret;
use voicetext_gateway::server::{
    GatewayLimits, GatewayReadiness, GatewayState, ReadinessFailure, reconcile_startup, router,
    start_startup_recovery,
};
use voicetext_speech::application::batch::{
    BatchAdmissionOutcome, BatchAdmissionRequest, BatchCoordinator,
};
use voicetext_speech::application::ports::{
    BatchAudioHandle, BatchAudioSpool, BatchAudioSpoolFailure, BatchAudioStoreOutcome, BatchJobId,
    BatchJobInsertOutcome, BatchJobSnapshot, BatchJobStore, BatchJobStoreFailure,
    BatchJobUpdateOutcome, BatchRecognitionRequest, BatchRecognitionResult, BatchRecognizer,
    BatchSegment, BoxFuture, ProviderReference, RecognitionFailure,
};
use voicetext_speech::domain::batch::{
    BatchJob, BatchJobState, BatchProfile, BatchRequestFingerprint,
};

const TOKEN: &str = "cleanup-metrics-test-token-000001";
const IDEMPOTENCY_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[derive(Debug, Default)]
struct CleanupInfrastructure {
    jobs: Mutex<BTreeMap<String, BatchJobSnapshot>>,
    audio: Mutex<BTreeMap<String, Vec<u8>>>,
    fail_remove: AtomicBool,
    remove_calls: AtomicUsize,
}

impl CleanupInfrastructure {
    fn with_remove_failure(fail_remove: bool) -> Self {
        Self {
            fail_remove: AtomicBool::new(fail_remove),
            ..Self::default()
        }
    }

    fn audio_present(&self) -> bool {
        !self.audio.lock().unwrap().is_empty()
    }
}

impl BatchJobStore for CleanupInfrastructure {
    fn load<'a>(
        &'a self,
        id: &'a BatchJobId,
    ) -> BoxFuture<'a, Result<Option<BatchJobSnapshot>, BatchJobStoreFailure>> {
        let snapshot = self.jobs.lock().unwrap().get(id.as_str()).cloned();
        Box::pin(async move { Ok(snapshot) })
    }

    fn insert(
        &self,
        id: BatchJobId,
        job: BatchJob,
        audio: BatchAudioHandle,
        authoritative_duration_millis: u64,
        keyterms: Vec<String>,
    ) -> BoxFuture<'_, Result<BatchJobInsertOutcome, BatchJobStoreFailure>> {
        let mut jobs = self.jobs.lock().unwrap();
        let outcome = if let Some(snapshot) = jobs.get(id.as_str()) {
            BatchJobInsertOutcome::Existing(snapshot.clone())
        } else {
            let snapshot = BatchJobSnapshot {
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
            jobs.insert(snapshot.id.as_str().into(), snapshot.clone());
            BatchJobInsertOutcome::Inserted(snapshot)
        };
        Box::pin(async move { Ok(outcome) })
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        mut replacement: BatchJobSnapshot,
    ) -> BoxFuture<'_, Result<BatchJobUpdateOutcome, BatchJobStoreFailure>> {
        let mut jobs = self.jobs.lock().unwrap();
        let outcome = match jobs.get_mut(replacement.id.as_str()) {
            None => BatchJobUpdateOutcome::Missing,
            Some(current) if current.revision != expected_revision => {
                BatchJobUpdateOutcome::RevisionConflict(current.clone())
            }
            Some(current) => {
                replacement.revision = expected_revision + 1;
                current.clone_from(&replacement);
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
            .values()
            .next_back()
            .map(|snapshot| snapshot.id.clone());
        Box::pin(async move { Ok(head) })
    }

    fn list_recovery_candidates(
        &self,
        after: Option<BatchJobId>,
        maximum: NonZeroUsize,
    ) -> BoxFuture<'_, Result<Vec<BatchJobSnapshot>, BatchJobStoreFailure>> {
        let jobs = self.jobs.lock().unwrap();
        let candidates = jobs
            .values()
            .filter(|snapshot| {
                after
                    .as_ref()
                    .is_none_or(|cursor| snapshot.id.as_str() > cursor.as_str())
            })
            .take(maximum.get())
            .cloned()
            .collect();
        Box::pin(async move { Ok(candidates) })
    }
}

impl BatchAudioSpool for CleanupInfrastructure {
    fn store(
        &self,
        id: BatchJobId,
        audio: Vec<u8>,
    ) -> BoxFuture<'_, Result<BatchAudioStoreOutcome, BatchAudioSpoolFailure>> {
        let handle = BatchAudioHandle::new(format!("{}.ogg", id.as_str()));
        let mut stored = self.audio.lock().unwrap();
        let outcome = match stored.get(handle.as_str()) {
            Some(existing) if existing == &audio => BatchAudioStoreOutcome::Existing(handle),
            Some(_) => return Box::pin(async { Err(BatchAudioSpoolFailure::IdentityConflict) }),
            None => {
                stored.insert(handle.as_str().into(), audio);
                BatchAudioStoreOutcome::Stored(handle)
            }
        };
        Box::pin(async move { Ok(outcome) })
    }

    fn read<'a>(
        &'a self,
        handle: &'a BatchAudioHandle,
    ) -> BoxFuture<'a, Result<Vec<u8>, BatchAudioSpoolFailure>> {
        let audio = self.audio.lock().unwrap().get(handle.as_str()).cloned();
        Box::pin(async move { audio.ok_or(BatchAudioSpoolFailure::Missing) })
    }

    fn remove<'a>(
        &'a self,
        handle: &'a BatchAudioHandle,
    ) -> BoxFuture<'a, Result<(), BatchAudioSpoolFailure>> {
        self.remove_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_remove.load(Ordering::SeqCst) {
            return Box::pin(async {
                Err(BatchAudioSpoolFailure::Unavailable {
                    code: "SYNTHETIC_REMOVE_FAILURE".into(),
                })
            });
        }
        self.audio.lock().unwrap().remove(handle.as_str());
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug, Default)]
struct CountingRecognizer {
    calls: AtomicUsize,
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
            text: "authoritative synthetic speech".into(),
            duration_millis: request.authoritative_duration_millis,
            provider_duration_millis: Some(request.authoritative_duration_millis),
            segments: vec![BatchSegment {
                start_millis: 0,
                end_millis: request.authoritative_duration_millis,
                text: "authoritative synthetic speech".into(),
                confidence: Some(0.99),
                speaker: None,
            }],
            readable_segments: None,
            provider_reference: Some(ProviderReference::new("cleanup-metrics-effect")),
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

fn state(
    infrastructure: Arc<CleanupInfrastructure>,
    recognizer: Arc<CountingRecognizer>,
) -> GatewayState {
    let jobs: Arc<dyn BatchJobStore> = infrastructure.clone();
    let spool: Arc<dyn BatchAudioSpool> = infrastructure;
    GatewayState::new(
        MachineSecret::from_token(TOKEN.as_bytes()).unwrap(),
        jobs,
        spool,
        ProfileRegistry::new().with_batch(recognizer),
        Arc::new(AlwaysReady),
        GatewayLimits::new(
            1024 * 1024,
            64 * 1024,
            NonZeroUsize::new(2).unwrap(),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn normal_cleanup_success_releases_custody_metrics() {
    assert_normal_cleanup(false, 1, 0, false).await;
}

#[tokio::test]
async fn normal_cleanup_failure_retains_authoritative_result_and_custody_metrics() {
    assert_normal_cleanup(true, 0, support::synthetic_ogg_opus().len(), true).await;
}

async fn assert_normal_cleanup(
    fail_remove: bool,
    expected_removed: usize,
    expected_used_bytes: usize,
    expected_audio_present: bool,
) {
    let infrastructure = Arc::new(CleanupInfrastructure::with_remove_failure(fail_remove));
    let recognizer = Arc::new(CountingRecognizer::default());
    let (origin, task) = serve(state(infrastructure.clone(), recognizer.clone())).await;
    let client = reqwest::Client::new();
    let response = submit(&client, &origin).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job_id = response.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for_completed(&client, &origin, &job_id).await;
    assert_metrics(&client, &origin, expected_removed, expected_used_bytes).await;
    assert_eq!(infrastructure.audio_present(), expected_audio_present);
    assert_eq!(infrastructure.remove_calls.load(Ordering::SeqCst), 1);
    assert_eq!(recognizer.calls.load(Ordering::SeqCst), 1);

    let replay = submit(&client, &origin).await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(recognizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(infrastructure.remove_calls.load(Ordering::SeqCst), 1);
    assert_metrics(&client, &origin, expected_removed, expected_used_bytes).await;
    task.abort();
}

#[tokio::test]
async fn recovery_cleanup_success_releases_custody_metrics() {
    assert_recovery_cleanup(false, 1, 0, false).await;
}

#[tokio::test]
async fn recovery_cleanup_failure_retains_authoritative_result_and_custody_metrics() {
    assert_recovery_cleanup(true, 0, 32, true).await;
}

async fn assert_recovery_cleanup(
    fail_remove: bool,
    expected_removed: usize,
    expected_used_bytes: usize,
    expected_audio_present: bool,
) {
    let infrastructure = Arc::new(CleanupInfrastructure::with_remove_failure(fail_remove));
    let recognizer = Arc::new(CountingRecognizer::default());
    let job_id = BatchJobId::new(Uuid::new_v4().hyphenated().to_string());
    let coordinator = BatchCoordinator::new(
        recognizer.as_ref(),
        infrastructure.as_ref(),
        infrastructure.as_ref(),
    );
    assert!(matches!(
        coordinator
            .admit(BatchAdmissionRequest {
                id: job_id.clone(),
                profile: BatchProfile::new(2, "deepgram", "nova-3", "multi").unwrap(),
                fingerprint: BatchRequestFingerprint::from_bytes([7; 32]),
                audio: vec![7; 32],
                authoritative_duration_millis: 100,
                keyterms: Vec::new(),
            })
            .await
            .unwrap(),
        BatchAdmissionOutcome::Accepted(_)
    ));
    let state = state(infrastructure.clone(), recognizer.clone());
    state.record_startup_metrics(0, 0, 0, 0, 32, 1024 * 1024);
    let recovery = reconcile_startup(&state).await.unwrap();
    start_startup_recovery(&state, recovery);
    wait_for_stored_completion(infrastructure.as_ref(), &job_id).await;
    let (origin, task) = serve(state.clone()).await;
    assert_metrics(
        &reqwest::Client::new(),
        &origin,
        expected_removed,
        expected_used_bytes,
    )
    .await;
    assert_eq!(infrastructure.audio_present(), expected_audio_present);
    assert_eq!(infrastructure.remove_calls.load(Ordering::SeqCst), 1);
    assert_eq!(recognizer.calls.load(Ordering::SeqCst), 1);

    let replay = reconcile_startup(&state).await.unwrap();
    start_startup_recovery(&state, replay);
    tokio::task::yield_now().await;
    assert_eq!(recognizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(infrastructure.remove_calls.load(Ordering::SeqCst), 1);
    assert!(
        infrastructure
            .load(&job_id)
            .await
            .unwrap()
            .unwrap()
            .result
            .is_some()
    );
    task.abort();
}

async fn serve(state: GatewayState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    (format!("http://{address}"), task)
}

async fn submit(client: &reqwest::Client, origin: &str) -> reqwest::Response {
    client
        .post(format!("{origin}/api/v1/transcribe/batch"))
        .bearer_auth(TOKEN)
        .header("x-idempotency-key", IDEMPOTENCY_KEY)
        .multipart(
            Form::new()
                .text("contract_version", "2")
                .text("provider", "deepgram")
                .text("model", "nova-3")
                .text("language", "multi")
                .text("keyterms", "[]")
                .part(
                    "file",
                    Part::bytes(support::synthetic_ogg_opus())
                        .file_name("audio.ogg")
                        .mime_str("audio/ogg")
                        .unwrap(),
                ),
        )
        .send()
        .await
        .unwrap()
}

async fn wait_for_completed(client: &reqwest::Client, origin: &str, job_id: &str) {
    for _ in 0..100 {
        let response = client
            .get(format!("{origin}/api/v1/transcribe/batch/{job_id}"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        if response.status() == StatusCode::OK {
            assert_eq!(
                response.json::<Value>().await.unwrap()["text"],
                "authoritative synthetic speech"
            );
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("normal execution did not persist its completed result");
}

async fn wait_for_stored_completion(infrastructure: &CleanupInfrastructure, id: &BatchJobId) {
    for _ in 0..100 {
        let snapshot = infrastructure.load(id).await.unwrap().unwrap();
        if matches!(snapshot.job.state(), BatchJobState::Completed { .. }) {
            assert!(snapshot.result.is_some());
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("recovery execution did not persist its completed result");
}

async fn assert_metrics(
    client: &reqwest::Client,
    origin: &str,
    terminal_removed: usize,
    used_bytes: usize,
) {
    let metrics = client
        .get(format!("{origin}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains(&format!(
        "voicetext_spool_terminal_removed_total {terminal_removed}\n"
    )));
    assert!(metrics.contains(&format!("voicetext_spool_used_bytes {used_bytes}\n")));
}
