//! Disposable-PostgreSQL regression for native observation result rollback compatibility.

use std::str::FromStr;

use serde::Deserialize;
use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;
use voicetext_gateway::storage::PostgresBatchJobStore;
use voicetext_speech::application::ports::{
    BatchAudioHandle, BatchJobId, BatchJobInsertOutcome, BatchJobStore, BatchJobUpdateOutcome,
    BatchRecognitionResult, BatchSegment, ProviderOperationKind, ProviderReference,
};
use voicetext_speech::domain::batch::{BatchJob, BatchProfile, BatchRequestFingerprint};

// Exact strict ResultRecord shape from rollback baseline b9f2c73.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackResultRecord {
    text: String,
    duration_millis: u64,
    provider_duration_millis: Option<u64>,
    segments: Vec<Value>,
    readable_segments: Option<Vec<Value>>,
}

#[tokio::test]
#[ignore = "requires VOICETEXT_TEST_DATABASE_URL for a disposable database"]
async fn old_strict_binary_reads_result_json_written_with_typed_native_operation() {
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
        .max_connections(2)
        .connect_with(options)
        .await
        .unwrap();
    PostgresBatchJobStore::migrate(&pool).await.unwrap();
    let store = PostgresBatchJobStore::new(pool.clone());
    let job_uuid = Uuid::new_v4();
    let id = BatchJobId::new(job_uuid.hyphenated().to_string());
    let profile = BatchProfile::new(2, "deepgram", "nova-3", "multi").unwrap();
    let job = BatchJob::accept(
        profile.clone(),
        BatchRequestFingerprint::from_bytes([31; 32]),
    );
    let audio = BatchAudioHandle::new(format!("{}-{}.ogg", id.as_str(), "a".repeat(64)));
    let BatchJobInsertOutcome::Inserted(mut snapshot) =
        store.insert(id, job, audio, 100, Vec::new()).await.unwrap()
    else {
        panic!("fresh UUID unexpectedly existed")
    };
    snapshot.job.begin_submission().unwrap();
    let BatchJobUpdateOutcome::Stored(mut snapshot) =
        store.compare_and_swap(0, snapshot).await.unwrap()
    else {
        panic!("submission fence was not stored")
    };
    let reference = ProviderReference::operation(ProviderOperationKind::RequestId, "request-31");
    snapshot.provider_reference = Some(reference.clone());
    snapshot.result = Some(BatchRecognitionResult {
        profile,
        text: "native result".into(),
        duration_millis: 100,
        provider_duration_millis: Some(99),
        segments: vec![BatchSegment {
            start_millis: 0,
            end_millis: 100,
            text: "native result".into(),
            confidence: Some(0.95),
            speaker: None,
        }],
        readable_segments: None,
        provider_reference: Some(reference),
    });
    snapshot.job.complete().unwrap();
    let BatchJobUpdateOutcome::Stored(_) = store.compare_and_swap(1, snapshot).await.unwrap()
    else {
        panic!("terminal native result was not stored")
    };

    let (legacy_json, stored_reference): (Value, String) = sqlx::query_as(
        "SELECT result_json, provider_reference FROM voicetext_batch_jobs WHERE job_id=$1",
    )
    .bind(job_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    let rollback: RollbackResultRecord = serde_json::from_value(legacy_json).unwrap();
    assert_eq!(rollback.text, "native result");
    assert_eq!(rollback.duration_millis, 100);
    assert_eq!(rollback.provider_duration_millis, Some(99));
    assert_eq!(rollback.segments.len(), 1);
    assert!(rollback.readable_segments.is_none());
    assert_eq!(stored_reference, "request-31");
}
