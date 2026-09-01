//! `PostgreSQL` implementation of the durable batch job ledger.

use std::num::NonZeroUsize;

use sqlx::{AssertSqlSafe, PgPool};
use voicetext_speech::application::ports::{
    BatchAudioHandle, BatchJobId, BatchJobInsertOutcome, BatchJobSnapshot, BatchJobStore,
    BatchJobStoreFailure, BatchJobUpdateOutcome, BoxFuture,
};
use voicetext_speech::domain::batch::BatchJob;

use super::records::{JobRecord, RecordError, WritableRecord};

const COLUMNS: &str = "job_id, contract_version, provider, model, language, fingerprint, \
audio_handle, authoritative_duration_millis, keyterms, state, attempt, failure_code, \
unknown_reason, provider_reference, retry_after_millis, result_json, revision";

/// PostgreSQL-backed durable batch ledger using optimistic revisions.
#[derive(Clone, Debug)]
pub struct PostgresBatchJobStore {
    pool: PgPool,
}

impl PostgresBatchJobStore {
    /// Injects the already-configured `PostgreSQL` connection pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Applies embedded gateway migrations.
    ///
    /// # Errors
    ///
    /// Returns the migration error without concealing its source chain.
    pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
        sqlx::migrate!("./migrations").run(pool).await
    }

    async fn load_uuid(
        &self,
        id: uuid::Uuid,
    ) -> Result<Option<BatchJobSnapshot>, BatchJobStoreFailure> {
        let sql = format!("SELECT {COLUMNS} FROM voicetext_batch_jobs WHERE job_id = $1");
        sqlx::query_as::<_, JobRecord>(AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_failure)?
            .map(BatchJobSnapshot::try_from)
            .transpose()
            .map_err(snapshot_failure)
    }

    async fn insert_inner(
        &self,
        id: BatchJobId,
        job: BatchJob,
        audio: BatchAudioHandle,
        authoritative_duration_millis: u64,
        keyterms: Vec<String>,
    ) -> Result<BatchJobInsertOutcome, BatchJobStoreFailure> {
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
        let record = WritableRecord::try_from(&snapshot).map_err(snapshot_failure)?;
        let result = sqlx::query(
            "INSERT INTO voicetext_batch_jobs (
                job_id, contract_version, provider, model, language, fingerprint,
                audio_handle, authoritative_duration_millis, keyterms, state, attempt,
                failure_code, unknown_reason, provider_reference, retry_after_millis,
                result_json, revision
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)
             ON CONFLICT (job_id) DO NOTHING",
        )
        .bind(record.job_id)
        .bind(record.contract_version)
        .bind(record.provider)
        .bind(record.model)
        .bind(record.language)
        .bind(record.fingerprint)
        .bind(record.audio_handle)
        .bind(record.authoritative_duration_millis)
        .bind(record.keyterms)
        .bind(record.state)
        .bind(record.attempt)
        .bind(record.failure_code)
        .bind(record.unknown_reason)
        .bind(record.provider_reference)
        .bind(record.retry_after_millis)
        .bind(record.result_json)
        .bind(record.revision)
        .execute(&self.pool)
        .await
        .map_err(database_failure)?;
        let loaded = self
            .load_uuid(record.job_id)
            .await?
            .ok_or_else(|| unavailable("INSERTED_ROW_MISSING"))?;
        Ok(if result.rows_affected() == 1 {
            BatchJobInsertOutcome::Inserted(loaded)
        } else {
            BatchJobInsertOutcome::Existing(loaded)
        })
    }

    async fn compare_and_swap_inner(
        &self,
        expected_revision: u64,
        replacement: BatchJobSnapshot,
    ) -> Result<BatchJobUpdateOutcome, BatchJobStoreFailure> {
        if replacement.revision != expected_revision {
            return Err(invalid_snapshot("REPLACEMENT_REVISION_MISMATCH"));
        }
        let record = WritableRecord::try_from(&replacement).map_err(snapshot_failure)?;
        let expected = i64::try_from(expected_revision)
            .map_err(|_| invalid_snapshot("INVALID_EXPECTED_REVISION"))?;
        let sql = format!(
            "UPDATE voicetext_batch_jobs SET
                state=$10, attempt=$11, failure_code=$12, unknown_reason=$13,
                provider_reference=$14, retry_after_millis=$15, result_json=$16,
                revision=revision+1,
                updated_at=clock_timestamp()
             WHERE job_id=$1 AND revision=$17
               AND contract_version=$2 AND provider=$3 AND model=$4 AND language=$5
               AND fingerprint=$6 AND audio_handle=$7
               AND authoritative_duration_millis=$8 AND keyterms=$9
             RETURNING {COLUMNS}"
        );
        let updated = sqlx::query_as::<_, JobRecord>(AssertSqlSafe(sql))
            .bind(record.job_id)
            .bind(record.contract_version)
            .bind(record.provider)
            .bind(record.model)
            .bind(record.language)
            .bind(record.fingerprint)
            .bind(record.audio_handle)
            .bind(record.authoritative_duration_millis)
            .bind(record.keyterms)
            .bind(record.state)
            .bind(record.attempt)
            .bind(record.failure_code)
            .bind(record.unknown_reason)
            .bind(record.provider_reference)
            .bind(record.retry_after_millis)
            .bind(record.result_json)
            .bind(expected)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_failure)?;
        if let Some(updated) = updated {
            return BatchJobSnapshot::try_from(updated)
                .map(BatchJobUpdateOutcome::Stored)
                .map_err(snapshot_failure);
        }

        let Some(current) = self.load_uuid(record.job_id).await? else {
            return Ok(BatchJobUpdateOutcome::Missing);
        };
        if current.revision == expected_revision && !same_immutable(&current, &replacement) {
            return Err(invalid_snapshot("IMMUTABLE_IDENTITY_MISMATCH"));
        }
        Ok(BatchJobUpdateOutcome::RevisionConflict(current))
    }

    async fn recovery_inner(
        &self,
        after: Option<BatchJobId>,
        maximum: NonZeroUsize,
    ) -> Result<Vec<BatchJobSnapshot>, BatchJobStoreFailure> {
        let limit =
            i64::try_from(maximum.get()).map_err(|_| invalid_snapshot("INVALID_RECOVERY_LIMIT"))?;
        let after = after
            .as_ref()
            .map(|id| canonical_uuid(id.as_str()))
            .transpose()?;
        let sql = format!(
            "SELECT {COLUMNS} FROM voicetext_batch_jobs
             WHERE state IN ('accepted', 'retryable', 'submitting')
               AND ($1::uuid IS NULL OR job_id > $1)
             ORDER BY job_id LIMIT $2"
        );
        sqlx::query_as::<_, JobRecord>(AssertSqlSafe(sql))
            .bind(after)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(database_failure)?
            .into_iter()
            .map(BatchJobSnapshot::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(snapshot_failure)
    }
}

impl BatchJobStore for PostgresBatchJobStore {
    fn load<'a>(
        &'a self,
        id: &'a BatchJobId,
    ) -> BoxFuture<'a, Result<Option<BatchJobSnapshot>, BatchJobStoreFailure>> {
        Box::pin(async move {
            let id = canonical_uuid(id.as_str())?;
            self.load_uuid(id).await
        })
    }

    fn insert(
        &self,
        id: BatchJobId,
        job: BatchJob,
        audio: BatchAudioHandle,
        authoritative_duration_millis: u64,
        keyterms: Vec<String>,
    ) -> BoxFuture<'_, Result<BatchJobInsertOutcome, BatchJobStoreFailure>> {
        Box::pin(self.insert_inner(id, job, audio, authoritative_duration_millis, keyterms))
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        replacement: BatchJobSnapshot,
    ) -> BoxFuture<'_, Result<BatchJobUpdateOutcome, BatchJobStoreFailure>> {
        Box::pin(self.compare_and_swap_inner(expected_revision, replacement))
    }

    fn list_recovery_candidates(
        &self,
        after: Option<BatchJobId>,
        maximum: NonZeroUsize,
    ) -> BoxFuture<'_, Result<Vec<BatchJobSnapshot>, BatchJobStoreFailure>> {
        Box::pin(self.recovery_inner(after, maximum))
    }
}

fn same_immutable(left: &BatchJobSnapshot, right: &BatchJobSnapshot) -> bool {
    left.id == right.id
        && left.job.profile() == right.job.profile()
        && left.job.fingerprint() == right.job.fingerprint()
        && left.audio == right.audio
        && left.authoritative_duration_millis == right.authoritative_duration_millis
        && left.keyterms == right.keyterms
}

fn canonical_uuid(value: &str) -> Result<uuid::Uuid, BatchJobStoreFailure> {
    let uuid = uuid::Uuid::parse_str(value).map_err(|_| invalid_snapshot("INVALID_JOB_ID"))?;
    if uuid.hyphenated().to_string() != value {
        return Err(invalid_snapshot("INVALID_JOB_ID"));
    }
    Ok(uuid)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "sqlx Result::map_err transfers ownership"
)]
fn database_failure(error: sqlx::Error) -> BatchJobStoreFailure {
    let code = error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .map_or_else(
            || "DATABASE_UNAVAILABLE".into(),
            |code| format!("DATABASE_{code}"),
        );
    unavailable(&code)
}

fn snapshot_failure(error: RecordError) -> BatchJobStoreFailure {
    invalid_snapshot(error.0)
}

fn unavailable(code: &str) -> BatchJobStoreFailure {
    BatchJobStoreFailure::Unavailable { code: code.into() }
}

fn invalid_snapshot(code: &str) -> BatchJobStoreFailure {
    BatchJobStoreFailure::InvalidSnapshot { code: code.into() }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::storage::records::{JobRecord, WritableRecord};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use voicetext_speech::application::ports::{
        BatchRecognitionResult, BatchSegment, ProviderReference,
    };
    use voicetext_speech::domain::batch::{BatchFailure, BatchProfile, BatchRequestFingerprint};

    fn snapshot() -> BatchJobSnapshot {
        let profile = BatchProfile::new(2, "deepgram", "nova-3", "multi").unwrap();
        let mut job = BatchJob::accept(
            profile.clone(),
            BatchRequestFingerprint::from_bytes([7; 32]),
        );
        job.begin_submission().unwrap();
        job.complete().unwrap();
        BatchJobSnapshot {
            id: BatchJobId::new("123e4567-e89b-12d3-a456-426614174000"),
            job,
            audio: BatchAudioHandle::new(format!(
                "123e4567-e89b-12d3-a456-426614174000-{}.ogg",
                "a".repeat(64)
            )),
            authoritative_duration_millis: 100,
            keyterms: vec!["voice".into()],
            provider_reference: Some(ProviderReference::new("request-1")),
            retry_after_millis: None,
            result: Some(BatchRecognitionResult {
                profile,
                text: "hello".into(),
                duration_millis: 100,
                provider_duration_millis: Some(100),
                segments: vec![BatchSegment {
                    start_millis: 0,
                    end_millis: 100,
                    text: "hello".into(),
                    confidence: Some(0.9),
                    speaker: None,
                }],
                readable_segments: None,
                provider_reference: Some(ProviderReference::new("request-1")),
            }),
            revision: 2,
        }
    }

    fn job_record(writable: WritableRecord) -> JobRecord {
        JobRecord {
            job_id: writable.job_id,
            contract_version: writable.contract_version,
            provider: writable.provider,
            model: writable.model,
            language: writable.language,
            fingerprint: writable.fingerprint,
            audio_handle: writable.audio_handle,
            authoritative_duration_millis: writable.authoritative_duration_millis,
            keyterms: writable.keyterms,
            state: writable.state.into(),
            attempt: writable.attempt,
            failure_code: writable.failure_code,
            unknown_reason: writable.unknown_reason.map(Into::into),
            provider_reference: writable.provider_reference,
            retry_after_millis: writable.retry_after_millis,
            result_json: writable.result_json,
            revision: writable.revision,
        }
    }

    #[test]
    fn only_canonical_uuid_job_ids_reach_sql() {
        assert!(canonical_uuid("123e4567-e89b-12d3-a456-426614174000").is_ok());
        assert!(canonical_uuid("123E4567-E89B-12D3-A456-426614174000").is_err());
        assert!(canonical_uuid("not-a-uuid").is_err());
    }

    #[test]
    fn record_round_trip_preserves_snapshot() {
        let expected = snapshot();
        let record = job_record(WritableRecord::try_from(&expected).unwrap());
        assert_eq!(BatchJobSnapshot::try_from(record).unwrap(), expected);
    }

    #[test]
    fn malformed_state_and_result_fail_closed() {
        let mut writable = WritableRecord::try_from(&snapshot()).unwrap();
        writable.attempt = None;
        assert_eq!(
            BatchJobSnapshot::try_from(job_record(writable)),
            Err(RecordError("INVALID_STATE_SNAPSHOT"))
        );

        let mut writable = WritableRecord::try_from(&snapshot()).unwrap();
        writable.result_json = Some(serde_json::json!({ "unexpected": true }));
        assert_eq!(
            BatchJobSnapshot::try_from(job_record(writable)),
            Err(RecordError("INVALID_RESULT_JSON"))
        );
    }

    #[test]
    fn retry_delay_round_trip_is_state_aware() {
        let mut expected = snapshot();
        let mut job = BatchJob::accept(expected.job.profile().clone(), expected.job.fingerprint());
        job.begin_submission().unwrap();
        job.record_retryable_failure(BatchFailure::new("CAPACITY_BUSY").unwrap())
            .unwrap();
        expected.job = job;
        expected.provider_reference = None;
        expected.retry_after_millis = Some(250);
        expected.result = None;
        let record = job_record(WritableRecord::try_from(&expected).unwrap());
        assert_eq!(BatchJobSnapshot::try_from(record).unwrap(), expected);
    }

    #[tokio::test]
    #[ignore = "requires VOICETEXT_TEST_DATABASE_URL for a disposable database"]
    async fn disposable_postgres_proves_insert_cas_and_recovery() {
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
        let store = PostgresBatchJobStore::new(pool);
        let id = BatchJobId::new(uuid::Uuid::new_v4().hyphenated().to_string());
        let profile = BatchProfile::new(2, "deepgram", "nova-3", "multi").unwrap();
        let job = BatchJob::accept(profile, BatchRequestFingerprint::from_bytes([9; 32]));
        let audio = BatchAudioHandle::new(format!("{}-{}.ogg", id.as_str(), "b".repeat(64)));
        let BatchJobInsertOutcome::Inserted(mut inserted) = store
            .insert(id.clone(), job, audio, 100, vec!["voice".into()])
            .await
            .unwrap()
        else {
            panic!("fresh UUID unexpectedly existed")
        };
        assert_eq!(inserted.revision, 0);
        inserted.job.begin_submission().unwrap();
        let BatchJobUpdateOutcome::Stored(updated) =
            store.compare_and_swap(0, inserted).await.unwrap()
        else {
            panic!("CAS did not store")
        };
        assert_eq!(updated.revision, 1);
        let first = store
            .list_recovery_candidates(None, NonZeroUsize::new(1).unwrap())
            .await
            .unwrap();
        assert!(first.len() <= 1);
        let after = store
            .list_recovery_candidates(Some(id.clone()), NonZeroUsize::new(1).unwrap())
            .await
            .unwrap();
        assert!(
            after.len() <= 1
                && after
                    .iter()
                    .all(|snapshot| snapshot.id.as_str() > id.as_str())
        );
    }
}
