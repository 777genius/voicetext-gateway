//! Upgrade and rollback compatibility proof for the `PostgreSQL` result representation.

use std::str::FromStr;

use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

const INITIAL: &str = include_str!("../migrations/0001_voicetext_batch_jobs.sql");
const EXPAND: &str = include_str!("../migrations/0002_exact_result_representation.sql");

#[tokio::test]
#[ignore = "requires VOICETEXT_TEST_DATABASE_URL for a disposable database"]
async fn populated_0001_expands_without_breaking_previous_binary_reads_or_writes() {
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
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::raw_sql("DROP TABLE IF EXISTS voicetext_batch_jobs")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(INITIAL).execute(&pool).await.unwrap();

    let id = Uuid::new_v4();
    let initial = result("from-0001");
    sqlx::query(
        "INSERT INTO voicetext_batch_jobs (
          job_id, contract_version, provider, model, language, fingerprint, audio_handle,
          authoritative_duration_millis, keyterms, state, attempt, result_json, revision
         ) VALUES ($1,2,'deepgram','nova-3','multi',$2,$3,100,'[]','completed',1,$4,2)",
    )
    .bind(id)
    .bind(vec![7_u8; 32])
    .bind(format!("{}-{}.ogg", id.hyphenated(), "a".repeat(64)))
    .bind(&initial)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(EXPAND).execute(&pool).await.unwrap();
    let column_type: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns
         WHERE table_name='voicetext_batch_jobs' AND column_name='result_json'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(column_type, "jsonb");
    let (legacy, exact, legacy_rendered): (Value, String, String) = sqlx::query_as(
        "SELECT result_json, result_text, result_json::text
         FROM voicetext_batch_jobs WHERE job_id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(legacy, initial);
    assert_eq!(exact, legacy_rendered);
    assert_eq!(serde_json::from_str::<Value>(&exact).unwrap(), initial);
    let dual_written = result("dual-written");
    let dual_text = serde_json::to_string(&dual_written).unwrap();
    sqlx::query("UPDATE voicetext_batch_jobs SET result_json=$2, result_text=$3 WHERE job_id=$1")
        .bind(id)
        .bind(&dual_written)
        .bind(&dual_text)
        .execute(&pool)
        .await
        .unwrap();
    let previous_binary_read: Value =
        sqlx::query_scalar("SELECT result_json FROM voicetext_batch_jobs WHERE job_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(previous_binary_read, dual_written);

    let rollback_written = result("rollback-write");
    sqlx::query("UPDATE voicetext_batch_jobs SET result_json=$2 WHERE job_id=$1")
        .bind(id)
        .bind(&rollback_written)
        .execute(&pool)
        .await
        .unwrap();
    let (legacy_after_rollback, stale_exact): (Value, String) =
        sqlx::query_as("SELECT result_json, result_text FROM voicetext_batch_jobs WHERE job_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(legacy_after_rollback, rollback_written);
    assert_eq!(stale_exact, dual_text);
    sqlx::raw_sql("DROP TABLE voicetext_batch_jobs")
        .execute(&pool)
        .await
        .unwrap();
}

fn result(text: &str) -> Value {
    json!({
        "text": text,
        "duration_millis": 100,
        "provider_duration_millis": 100,
        "segments": [],
        "readable_segments": null
    })
}
