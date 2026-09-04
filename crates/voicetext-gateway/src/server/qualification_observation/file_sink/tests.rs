use super::*;
use crate::server::qualification_observation::{
    LiveObservationSink, LiveObservationTracker, ObservationProfile,
};
use std::os::unix::fs::PermissionsExt;
use tokio::sync::Notify;

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

fn private_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn assert_empty(directory: &Path) {
    assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
}

async fn wait_until_empty(directory: &Path) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while std::fs::read_dir(directory).unwrap().next().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

struct BlockingGate {
    stage: BlockingStage,
    entered: Notify,
    released: Mutex<bool>,
    changed: Condvar,
}

impl BlockingGate {
    fn new(stage: BlockingStage) -> Arc<Self> {
        Arc::new(Self {
            stage,
            entered: Notify::new(),
            released: Mutex::new(false),
            changed: Condvar::new(),
        })
    }

    fn hook(self: &Arc<Self>) -> BlockingHook {
        let gate = Arc::clone(self);
        Arc::new(move |stage| {
            if stage != gate.stage {
                return;
            }
            gate.entered.notify_one();
            let mut released = gate.released.lock().unwrap();
            while !*released {
                released = gate.changed.wait(released).unwrap();
            }
        })
    }

    fn release(&self) {
        let mut released = self.released.lock().unwrap();
        *released = true;
        self.changed.notify_all();
    }
}

#[test]
fn receipt_wait_cannot_lose_accept_notification() {
    let receipt = Arc::new(PublicationReceipt::new());
    let (at_wait_sender, at_wait_receiver) = std::sync::mpsc::channel();
    let (release_wait_sender, release_wait_receiver) = std::sync::mpsc::channel();
    let (waited_sender, waited_receiver) = std::sync::mpsc::channel();
    let waiting_receipt = Arc::clone(&receipt);
    let waiter = std::thread::spawn(move || {
        let state = waiting_receipt.wait_for_caller_after(|| {
            at_wait_sender.send(()).unwrap();
            release_wait_receiver.recv().unwrap();
        });
        waited_sender.send(state).unwrap();
    });

    at_wait_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let accepting_receipt = Arc::clone(&receipt);
    let (accepting_sender, accepting_receiver) = std::sync::mpsc::channel();
    let accepter = std::thread::spawn(move || {
        accepting_sender.send(()).unwrap();
        accepting_receipt.accept(Instant::now() + Duration::from_secs(5))
    });
    accepting_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    release_wait_sender.send(()).unwrap();

    let waited = waited_receiver.recv_timeout(Duration::from_secs(1));
    if waited.is_err() {
        receipt.cancel();
    }
    assert_eq!(waited.unwrap(), ReceiptState::Accepted);
    assert!(accepter.join().unwrap());
    waiter.join().unwrap();
}

#[test]
fn caller_delayed_past_absolute_deadline_cancels_ready_receipt() {
    let receipt = PublicationReceipt::new();
    let expired = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .unwrap();

    // Worker readiness deliberately leaves the receipt active for caller acceptance.
    assert!(!receipt.accept(expired));
    assert_eq!(receipt.wait_for_caller(), ReceiptState::Cancelled);
}

#[tokio::test]
async fn cancellation_before_publication_leaves_no_late_artifact() {
    let directory = private_directory();
    let gate = BlockingGate::new(BlockingStage::BeforePublish);
    let sink = Arc::new(
        FileQualificationSink::open(
            directory.path(),
            "cancelled",
            Duration::from_secs(1),
            gate.hook(),
        )
        .unwrap(),
    );
    let task = tokio::spawn({
        let sink = Arc::clone(&sink);
        async move {
            tokio::time::timeout(
                Duration::from_millis(20),
                sink.observe_live(record(Uuid::from_u128(3))),
            )
            .await
        }
    });
    gate.entered.notified().await;
    assert!(task.await.unwrap().is_err());
    gate.release();
    wait_until_empty(directory.path()).await;
}

#[tokio::test]
async fn delayed_publication_cannot_be_qualified_after_caller_deadline() {
    let directory = private_directory();
    let gate = BlockingGate::new(BlockingStage::AfterPublish);
    let effect = Uuid::from_u128(4);
    let sink = Arc::new(
        FileQualificationSink::open(
            directory.path(),
            "deadline",
            Duration::from_secs(1),
            gate.hook(),
        )
        .unwrap(),
    );
    let task = tokio::spawn({
        let sink = Arc::clone(&sink);
        async move {
            tokio::time::timeout(Duration::from_millis(20), sink.observe_live(record(effect))).await
        }
    });
    gate.entered.notified().await;
    assert!(
        directory
            .path()
            .join(format!("deadline-live-{effect}.json"))
            .exists()
    );
    assert!(task.await.unwrap().is_err());
    gate.release();
    wait_until_empty(directory.path()).await;
}

#[tokio::test]
async fn worker_deadline_rejects_a_delayed_blocking_operation() {
    let directory = private_directory();
    let gate = BlockingGate::new(BlockingStage::BeforeCreate);
    let sink = Arc::new(
        FileQualificationSink::open(
            directory.path(),
            "worker_deadline",
            Duration::from_millis(20),
            gate.hook(),
        )
        .unwrap(),
    );
    let task = tokio::spawn({
        let sink = Arc::clone(&sink);
        async move { sink.observe_live(record(Uuid::from_u128(5))).await }
    });
    gate.entered.notified().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    gate.release();
    assert_eq!(
        task.await.unwrap(),
        Err(ObservationSinkFailure("QUALIFICATION_WRITE_TIMEOUT"))
    );
    assert_empty(directory.path());
}

#[tokio::test]
async fn publication_error_cleans_temporary_file_and_preserves_existing_record() {
    let directory = private_directory();
    let sink = FileQualificationSink::new(directory.path(), "campaign").unwrap();
    let effect = Uuid::from_u128(6);
    sink.observe_live(record(effect)).await.unwrap();
    let path = directory
        .path()
        .join(format!("campaign-live-{effect}.json"));
    let original = std::fs::read(&path).unwrap();
    assert_eq!(
        sink.observe_live(record(effect)).await,
        Err(ObservationSinkFailure("QUALIFICATION_CREATE_FAILED"))
    );
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.')
    }));
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

    let effect = Uuid::from_u128(7);
    sink.observe_live(record(effect)).await.unwrap();
    assert!(
        !original
            .join(format!("campaign-live-{effect}.json"))
            .exists()
    );
    assert!(pinned.join(format!("campaign-live-{effect}.json")).exists());
}

#[tokio::test]
async fn process_record_limit_is_exact_and_leaves_no_temporary_names() {
    let directory = private_directory();
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
}
