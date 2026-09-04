use super::*;
use crate::server::qualification_observation::{
    LiveObservationSink, LiveObservationTracker, ObservationProfile,
};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::AsyncWrite;

struct PendingWriter;

impl AsyncWrite for PendingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn paths(directory: &Path) -> (PathBuf, PathBuf) {
    (
        directory.join("record.pending"),
        directory.join("record.json"),
    )
}

fn guard(directory: &Path) -> PublicationGuard {
    let (temporary, final_path) = paths(directory);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .unwrap();
    PublicationGuard::new(file, temporary, final_path).unwrap()
}

fn assert_empty(directory: &Path) {
    assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
}

fn record(effect_id: Uuid) -> LiveObservation {
    let mut record = LiveObservationTracker::new(
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        ObservationProfile {
            contract_version: 2,
            provider: "deepgram".into(),
            model: "nova-3".into(),
            language: "multi".into(),
        },
    )
    .finish(None, "client_close".into());
    record.effect_id = effect_id;
    record
}

#[tokio::test]
async fn cancellation_during_write_removes_only_the_private_temporary_inode() {
    let directory = tempfile::tempdir().unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(1),
        write_stage(guard(directory.path()), PendingWriter, b"record"),
    )
    .await;
    assert!(result.is_err());
    assert_empty(directory.path());
}

#[tokio::test]
async fn cancellation_during_file_sync_removes_the_private_temporary_inode() {
    let directory = tempfile::tempdir().unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(1),
        sync_stage(
            guard(directory.path()),
            std::future::pending::<std::io::Result<()>>(),
        ),
    )
    .await;
    assert!(result.is_err());
    assert_empty(directory.path());
}

#[tokio::test]
async fn cancellation_during_directory_sync_removes_unqualified_publication() {
    let directory = tempfile::tempdir().unwrap();
    let mut publication = guard(directory.path());
    publication.publish().unwrap();
    publication.remove_temporary().unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(1),
        sync_stage(publication, std::future::pending::<std::io::Result<()>>()),
    )
    .await;
    assert!(result.is_err());
    assert_empty(directory.path());
}

#[test]
fn cleanup_never_unlinks_a_replaced_foreign_inode() {
    let directory = tempfile::tempdir().unwrap();
    let publication = guard(directory.path());
    let (temporary, _) = paths(directory.path());
    std::fs::remove_file(&temporary).unwrap();
    std::fs::write(&temporary, b"foreign").unwrap();
    drop(publication);
    assert_eq!(std::fs::read(&temporary).unwrap(), b"foreign");
}

#[tokio::test]
async fn publication_is_create_only_and_uses_pinned_directory_custody() {
    let parent = tempfile::tempdir().unwrap();
    let original = parent.path().join("observations");
    let pinned = parent.path().join("pinned");
    std::fs::create_dir(&original).unwrap();
    std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sink = FileQualificationSink::new(&original, "campaign").unwrap();
    std::fs::rename(&original, &pinned).unwrap();
    std::fs::create_dir(&original).unwrap();
    std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();

    let effect = Uuid::from_u128(3);
    let record = record(effect);
    sink.observe_live(record.clone()).await.unwrap();
    assert!(
        !original
            .join(format!("campaign-live-{effect}.json"))
            .exists()
    );
    assert!(pinned.join(format!("campaign-live-{effect}.json")).exists());
    assert_eq!(
        sink.observe_live(record).await,
        Err(ObservationSinkFailure("QUALIFICATION_CREATE_FAILED"))
    );
    assert!(std::fs::read_dir(&pinned).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.')
    }));
}

#[tokio::test]
async fn process_record_limit_is_exact_and_leaves_no_temporary_names() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let sink = FileQualificationSink::new(directory.path(), "bounded").unwrap();
    for value in 1..=MAX_OBSERVATIONS {
        sink.observe_live(record(Uuid::from_u128(value as u128)))
            .await
            .unwrap();
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 64);
    assert_eq!(
        sink.observe_live(record(Uuid::from_u128(65))).await,
        Err(ObservationSinkFailure("QUALIFICATION_RECORD_LIMIT"))
    );
    assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.')
    }));
}
