use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use voicetext_speech::application::ports::{
    BatchAudioHandle, BatchAudioSpool, BatchAudioSpoolFailure, BatchAudioStoreOutcome, BatchJobId,
    BatchJobInsertOutcome, BatchJobSnapshot, BatchJobStore, BatchJobStoreFailure,
    BatchJobUpdateOutcome, BatchReadableSegment, BatchRecognitionRequest, BatchRecognitionResult,
    BatchRecognizer, BatchSegment, BoxFuture, ProviderReference, RecognitionFailure,
};
use voicetext_speech::domain::batch::BatchJob;

#[derive(Debug, Default)]
pub struct FakeBatchInfrastructure {
    jobs: Mutex<BTreeMap<String, BatchJobSnapshot>>,
    audio: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl BatchJobStore for FakeBatchInfrastructure {
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

impl BatchAudioSpool for FakeBatchInfrastructure {
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
        self.audio.lock().unwrap().remove(handle.as_str());
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug)]
pub struct FakeBatchRecognizer;

impl BatchRecognizer for FakeBatchRecognizer {
    fn recognize(
        &self,
        request: BatchRecognitionRequest,
    ) -> BoxFuture<'_, Result<BatchRecognitionResult, RecognitionFailure>> {
        let result = BatchRecognitionResult {
            profile: request.profile.clone(),
            text: "synthetic speech".into(),
            duration_millis: request.authoritative_duration_millis,
            provider_duration_millis: Some(request.authoritative_duration_millis),
            segments: vec![BatchSegment {
                start_millis: 0,
                end_millis: request.authoritative_duration_millis,
                text: "synthetic speech".into(),
                confidence: Some(0.95),
                speaker: None,
            }],
            readable_segments: (request.profile.contract_version() == 2).then(|| {
                vec![BatchReadableSegment {
                    start_millis: 0,
                    end_millis: request.authoritative_duration_millis,
                    text: "synthetic speech".into(),
                    source_segment_indices: vec![0],
                }]
            }),
            provider_reference: (request.profile.contract_version() == 3)
                .then(|| ProviderReference::new("fake-request-1")),
        };
        Box::pin(async move { Ok(result) })
    }
}
