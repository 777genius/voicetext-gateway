//! Create-only native qualification record publication on an isolated blocking worker.

use super::{
    BatchObservation, BatchObservationSink, LiveObservation, LiveObservationSink,
    OBSERVATION_WRITE_TIMEOUT, ObservationFuture, ObservationSinkFailure, valid_campaign,
};
use serde::Serialize;
use std::fmt;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use uuid::Uuid;

const MAX_OBSERVATIONS: usize = 64;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const RECEIPT_ACTIVE: u8 = 0;
const RECEIPT_CANCELLED: u8 = 1;
const RECEIPT_ACCEPTED: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockingStage {
    BeforeCreate,
    BeforePublish,
    AfterPublish,
}

#[cfg(test)]
type BlockingHook = Arc<dyn Fn(BlockingStage) + Send + Sync>;
#[cfg(not(test))]
type BlockingHook = ();

pub struct FileQualificationSink {
    directory: PathBuf,
    directory_handle: Option<Arc<std::fs::File>>,
    campaign: Box<str>,
    written: AtomicUsize,
    deadline: Duration,
    hook: BlockingHook,
}

impl fmt::Debug for FileQualificationSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileQualificationSink")
            .field("directory", &self.directory)
            .field("campaign", &self.campaign)
            .finish_non_exhaustive()
    }
}

impl FileQualificationSink {
    /// Opens and pins a validated private output directory for one bounded campaign.
    ///
    /// # Errors
    ///
    /// Returns a safe failure code when the campaign or directory custody is invalid.
    pub fn new(directory: &Path, campaign: &str) -> Result<Self, ObservationSinkFailure> {
        Self::open(
            directory,
            campaign,
            OBSERVATION_WRITE_TIMEOUT,
            default_hook(),
        )
    }

    fn open(
        directory: &Path,
        campaign: &str,
        deadline: Duration,
        hook: BlockingHook,
    ) -> Result<Self, ObservationSinkFailure> {
        if !valid_campaign(campaign) || !directory.is_absolute() {
            return Err(ObservationSinkFailure(
                "INVALID_QUALIFICATION_CONFIGURATION",
            ));
        }
        let metadata = std::fs::symlink_metadata(directory)
            .map_err(|_| ObservationSinkFailure("QUALIFICATION_DIRECTORY_UNAVAILABLE"))?;
        let self_uid = std::fs::metadata("/proc/self")
            .map_err(|_| ObservationSinkFailure("QUALIFICATION_CUSTODY_UNAVAILABLE"))?
            .uid();
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != self_uid
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(ObservationSinkFailure("QUALIFICATION_DIRECTORY_UNSAFE"));
        }
        let directory_handle = std::fs::File::open(directory)
            .map_err(|_| ObservationSinkFailure("QUALIFICATION_DIRECTORY_UNAVAILABLE"))?;
        let opened = directory_handle
            .metadata()
            .map_err(|_| ObservationSinkFailure("QUALIFICATION_DIRECTORY_UNAVAILABLE"))?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(ObservationSinkFailure("QUALIFICATION_DIRECTORY_UNSAFE"));
        }
        Ok(Self {
            directory: directory.to_owned(),
            directory_handle: Some(Arc::new(directory_handle)),
            campaign: campaign.into(),
            written: AtomicUsize::new(0),
            deadline,
            hook,
        })
    }

    fn write<T: Serialize + Send + 'static>(
        &self,
        mode: &'static str,
        effect_id: Uuid,
        record: T,
    ) -> ObservationFuture<'_> {
        let slot = self.written.fetch_add(1, Ordering::AcqRel);
        if slot >= MAX_OBSERVATIONS {
            self.written.fetch_sub(1, Ordering::AcqRel);
            return Box::pin(async { Err(ObservationSinkFailure("QUALIFICATION_RECORD_LIMIT")) });
        }
        let directory = PathBuf::from(format!(
            "/proc/self/fd/{}",
            self.directory_handle().as_raw_fd()
        ));
        let directory_handle = Arc::clone(self.directory_handle());
        let final_path = directory.join(format!("{}-{mode}-{effect_id}.json", self.campaign));
        let temporary_path = directory.join(format!(
            ".{}-{mode}-{effect_id}-{}.pending",
            self.campaign,
            Uuid::new_v4()
        ));
        let deadline = Instant::now() + self.deadline;
        let hook = self.hook.clone();
        Box::pin(async move {
            let receipt = Arc::new(PublicationReceipt::new());
            let cancellation = CallerCancellation(Arc::clone(&receipt));
            let (ready, prepared) = oneshot::channel();
            let worker_receipt = Arc::clone(&receipt);
            let _worker = tokio::task::spawn_blocking(move || {
                let _directory_handle = directory_handle;
                match prepare_record(
                    record,
                    directory,
                    temporary_path,
                    final_path,
                    deadline,
                    &worker_receipt,
                    &hook,
                ) {
                    Ok(mut guard) => {
                        if ready.send(Ok(())).is_err() {
                            worker_receipt.cancel();
                        }
                        guard.complete_when_accepted(&worker_receipt);
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            });

            let result = prepared
                .await
                .unwrap_or(Err(ObservationSinkFailure("QUALIFICATION_WRITE_FAILED")));
            if result.is_ok() && !cancellation.accept() {
                return Err(ObservationSinkFailure("QUALIFICATION_WRITE_TIMEOUT"));
            }
            result
        })
    }

    fn directory_handle(&self) -> &Arc<std::fs::File> {
        self.directory_handle
            .as_ref()
            .expect("qualification directory handle is present until sink drop")
    }
}

impl Drop for FileQualificationSink {
    fn drop(&mut self) {
        let Some(directory_handle) = self.directory_handle.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let _worker = runtime.spawn_blocking(move || drop(directory_handle));
        } else {
            drop(directory_handle);
        }
    }
}

fn prepare_record<T: Serialize>(
    record: T,
    directory: PathBuf,
    temporary_path: PathBuf,
    final_path: PathBuf,
    deadline: Instant,
    receipt: &PublicationReceipt,
    hook: &BlockingHook,
) -> Result<PublicationGuard, ObservationSinkFailure> {
    let bytes = serde_json::to_vec(&record)
        .map_err(|_| ObservationSinkFailure("QUALIFICATION_SERIALIZE_FAILED"))?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ObservationSinkFailure("QUALIFICATION_RECORD_TOO_LARGE"));
    }
    checkpoint(receipt, deadline)?;
    run_hook(hook, BlockingStage::BeforeCreate);
    checkpoint(receipt, deadline)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary_path)
        .map_err(|_| ObservationSinkFailure("QUALIFICATION_CREATE_FAILED"))?;
    let mut guard = PublicationGuard::new(file, temporary_path, final_path)?;
    checkpoint(receipt, deadline)?;
    guard
        .file
        .write_all(&bytes)
        .map_err(|_| ObservationSinkFailure("QUALIFICATION_WRITE_FAILED"))?;
    checkpoint(receipt, deadline)?;
    guard
        .file
        .sync_all()
        .map_err(|_| ObservationSinkFailure("QUALIFICATION_SYNC_FAILED"))?;
    checkpoint(receipt, deadline)?;
    run_hook(hook, BlockingStage::BeforePublish);
    checkpoint(receipt, deadline)?;
    guard.publish()?;
    run_hook(hook, BlockingStage::AfterPublish);
    checkpoint(receipt, deadline)?;
    guard.remove_temporary()?;
    checkpoint(receipt, deadline)?;
    let directory_file = std::fs::File::open(directory)
        .map_err(|_| ObservationSinkFailure("QUALIFICATION_SYNC_FAILED"))?;
    checkpoint(receipt, deadline)?;
    directory_file
        .sync_all()
        .map_err(|_| ObservationSinkFailure("QUALIFICATION_SYNC_FAILED"))?;
    checkpoint(receipt, deadline)?;
    Ok(guard)
}

fn checkpoint(
    receipt: &PublicationReceipt,
    deadline: Instant,
) -> Result<(), ObservationSinkFailure> {
    if receipt.is_active() && Instant::now() <= deadline {
        Ok(())
    } else {
        Err(ObservationSinkFailure("QUALIFICATION_WRITE_TIMEOUT"))
    }
}

struct PublicationReceipt {
    state: AtomicU8,
    mutex: Mutex<()>,
    changed: Condvar,
}

impl PublicationReceipt {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(RECEIPT_ACTIVE),
            mutex: Mutex::new(()),
            changed: Condvar::new(),
        }
    }

    fn is_active(&self) -> bool {
        self.state.load(Ordering::Acquire) == RECEIPT_ACTIVE
    }

    fn accept(&self) -> bool {
        let accepted = self
            .state
            .compare_exchange(
                RECEIPT_ACTIVE,
                RECEIPT_ACCEPTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        self.changed.notify_all();
        accepted
    }

    fn cancel(&self) {
        let _ = self.state.compare_exchange(
            RECEIPT_ACTIVE,
            RECEIPT_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.changed.notify_all();
    }

    fn wait_for_caller(&self) {
        let mut guard = self.mutex.lock().unwrap_or_else(|error| error.into_inner());
        while self.is_active() {
            guard = self
                .changed
                .wait(guard)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

struct CallerCancellation(Arc<PublicationReceipt>);

impl CallerCancellation {
    fn accept(&self) -> bool {
        self.0.accept()
    }
}

impl Drop for CallerCancellation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct PublicationGuard {
    file: std::fs::File,
    identity: (u64, u64),
    temporary_path: PathBuf,
    final_path: PathBuf,
    published: bool,
    complete: bool,
}

impl PublicationGuard {
    fn new(
        file: std::fs::File,
        temporary_path: PathBuf,
        final_path: PathBuf,
    ) -> Result<Self, ObservationSinkFailure> {
        let metadata = file
            .metadata()
            .map_err(|_| ObservationSinkFailure("QUALIFICATION_CREATE_FAILED"))?;
        Ok(Self {
            file,
            identity: (metadata.dev(), metadata.ino()),
            temporary_path,
            final_path,
            published: false,
            complete: false,
        })
    }

    fn publish(&mut self) -> Result<(), ObservationSinkFailure> {
        std::fs::hard_link(&self.temporary_path, &self.final_path)
            .map_err(|_| ObservationSinkFailure("QUALIFICATION_CREATE_FAILED"))?;
        self.published = true;
        Ok(())
    }

    fn remove_temporary(&mut self) -> Result<(), ObservationSinkFailure> {
        if remove_if_owned(&self.temporary_path, self.identity) {
            Ok(())
        } else {
            Err(ObservationSinkFailure("QUALIFICATION_SYNC_FAILED"))
        }
    }

    fn complete_when_accepted(&mut self, receipt: &PublicationReceipt) {
        receipt.wait_for_caller();
        self.complete = receipt.state.load(Ordering::Acquire) == RECEIPT_ACCEPTED;
    }
}

impl Drop for PublicationGuard {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        if self.published {
            remove_if_owned(&self.final_path, self.identity);
        }
        remove_if_owned(&self.temporary_path, self.identity);
    }
}

fn remove_if_owned(path: &Path, identity: (u64, u64)) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return !path.exists();
    };
    metadata.file_type().is_file()
        && (metadata.dev(), metadata.ino()) == identity
        && std::fs::remove_file(path).is_ok()
}

#[cfg(test)]
fn default_hook() -> BlockingHook {
    Arc::new(|_| {})
}

#[cfg(not(test))]
fn default_hook() -> BlockingHook {}

#[cfg(test)]
fn run_hook(hook: &BlockingHook, stage: BlockingStage) {
    hook(stage);
}

#[cfg(not(test))]
fn run_hook(_hook: &BlockingHook, _stage: BlockingStage) {}

impl BatchObservationSink for FileQualificationSink {
    fn enabled(&self) -> bool {
        true
    }

    fn observe_batch(&self, record: BatchObservation) -> ObservationFuture<'_> {
        self.write("batch", record.effect_id, record)
    }
}

impl LiveObservationSink for FileQualificationSink {
    fn enabled(&self) -> bool {
        true
    }

    fn observe_live(&self, record: LiveObservation) -> ObservationFuture<'_> {
        self.write("live", record.effect_id, record)
    }
}

#[cfg(test)]
mod tests;
