mod support;

use std::sync::atomic::{AtomicUsize, Ordering};

use uuid::Uuid;
use voicetext_gateway::contracts::batch::BatchIdentity;
use voicetext_gateway::contracts::batch_projection::GatewayBatchResultProjection;
use voicetext_speech::application::batch::{
    BatchAdmissionOutcome, BatchAdmissionRequest, BatchCoordinator, BatchExecutionOutcome,
};
use voicetext_speech::application::ports::{
    BatchAudioSpool, BatchJobId, BatchJobStore, BatchRecognitionRequest, BatchRecognitionResult,
    BatchRecognizer, BoxFuture, RecognitionFailure,
};
use voicetext_speech::domain::batch::{BatchJobState, BatchProfile, BatchRequestFingerprint};

use support::batch::FakeBatchInfrastructure;

#[derive(Debug)]
struct OversizedRecognizer {
    text: String,
    calls: AtomicUsize,
}

impl OversizedRecognizer {
    fn new(text: String) -> Self {
        Self {
            text,
            calls: AtomicUsize::new(0),
        }
    }
}

impl BatchRecognizer for OversizedRecognizer {
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
            text: self.text.clone(),
            duration_millis: request.authoritative_duration_millis,
            provider_duration_millis: Some(request.authoritative_duration_millis),
            segments: Vec::new(),
            readable_segments: None,
            provider_reference: None,
        };
        Box::pin(async move { Ok(result) })
    }
}

#[tokio::test]
async fn outbound_limits_become_terminal_before_completion_and_clean_spool() {
    let cases = [
        (Uuid::from_u128(1), "😀".repeat(500_001)),
        (Uuid::from_u128(2), "\u{0001}".repeat(350_000)),
    ];

    for (job_uuid, text) in cases {
        let infrastructure = FakeBatchInfrastructure::default();
        let recognizer = OversizedRecognizer::new(text);
        let coordinator = BatchCoordinator::new(&recognizer, &infrastructure, &infrastructure);
        let id = BatchJobId::new(job_uuid.hyphenated().to_string());
        let request = || BatchAdmissionRequest {
            id: id.clone(),
            profile: BatchProfile::new(2, "deepgram", "nova-3", "multi").unwrap(),
            fingerprint: BatchRequestFingerprint::from_bytes([1; 32]),
            audio: vec![1, 2, 3],
            authoritative_duration_millis: 100,
            keyterms: Vec::new(),
        };
        let admitted = coordinator.admit(request()).await.unwrap();
        let audio = match admitted {
            BatchAdmissionOutcome::Accepted(snapshot) => snapshot.audio,
            BatchAdmissionOutcome::Replay(_) => panic!("first admission must be new"),
        };

        let persisted = coordinator
            .execute(&id, &GatewayBatchResultProjection)
            .await
            .unwrap();
        let snapshot = match persisted {
            BatchExecutionOutcome::Persisted(snapshot) => snapshot,
            other => panic!("unexpected execution outcome: {other:?}"),
        };
        assert!(matches!(
            snapshot.job.state(),
            BatchJobState::Failed { failure, .. }
                if failure.code() == "PROVIDER_RESULT_PROJECTION_FAILED"
        ));
        assert!(snapshot.result.is_none());
        assert!(matches!(
            infrastructure.read(&audio).await,
            Err(voicetext_speech::application::ports::BatchAudioSpoolFailure::Missing)
        ));

        assert!(matches!(
            coordinator.admit(request()).await.unwrap(),
            BatchAdmissionOutcome::Replay(snapshot)
                if matches!(snapshot.job.state(), BatchJobState::Failed { .. })
        ));
        assert!(matches!(
            coordinator
                .execute(&id, &GatewayBatchResultProjection)
                .await
                .unwrap(),
            BatchExecutionOutcome::NotActionable(_)
        ));
        assert_eq!(recognizer.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            infrastructure.load(&id).await.unwrap().unwrap().job.state(),
            BatchJobState::Failed { .. }
        ));
    }
}
