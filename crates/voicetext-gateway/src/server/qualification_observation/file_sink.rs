//! Cancellation-safe, create-only native qualification record publication.

use super::{
    BatchObservation, BatchObservationSink, LiveObservation, LiveObservationSink,
    ObservationFuture, ObservationSinkFailure, valid_campaign,
};
use serde::Serialize;
use std::fmt;
use std::future::Future;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

const MAX_OBSERVATIONS: usize = 64;
const MAX_RECORD_BYTES: usize = 16 * 1024;

pub struct FileQualificationSink {
    directory: PathBuf,
    directory_handle: std::fs::File,
    campaign: Box<str>,
    written: AtomicUsize,
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
            directory_handle,
            campaign: campaign.into(),
            written: AtomicUsize::new(0),
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
            self.directory_handle.as_raw_fd()
        ));
        let final_path = directory.join(format!("{}-{mode}-{effect_id}.json", self.campaign));
        let temporary_path = directory.join(format!(
            ".{}-{mode}-{effect_id}-{}.pending",
            self.campaign,
            Uuid::new_v4()
        ));
        Box::pin(async move {
            let bytes = serde_json::to_vec(&record)
                .map_err(|_| ObservationSinkFailure("QUALIFICATION_SERIALIZE_FAILED"))?;
            if bytes.len() > MAX_RECORD_BYTES {
                return Err(ObservationSinkFailure("QUALIFICATION_RECORD_TOO_LARGE"));
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary_path)
                .map_err(|_| ObservationSinkFailure("QUALIFICATION_CREATE_FAILED"))?;
            let guard = PublicationGuard::new(file, temporary_path, final_path)?;
            persist_record(guard, bytes, directory).await
        })
    }
}

async fn persist_record(
    guard: PublicationGuard,
    bytes: Vec<u8>,
    directory: PathBuf,
) -> Result<(), ObservationSinkFailure> {
    let writer = tokio::fs::File::from_std(
        guard
            .file
            .try_clone()
            .map_err(|_| ObservationSinkFailure("QUALIFICATION_CREATE_FAILED"))?,
    );
    let (guard, writer) = write_stage(guard, writer, &bytes).await?;
    let mut guard = sync_stage(guard, writer.sync_all()).await?;
    drop(writer);
    guard.publish()?;
    guard.remove_temporary()?;
    let directory_file = tokio::fs::File::open(directory)
        .await
        .map_err(|_| ObservationSinkFailure("QUALIFICATION_SYNC_FAILED"))?;
    let guard = sync_stage(guard, directory_file.sync_all()).await?;
    guard.complete();
    Ok(())
}

async fn write_stage<W: AsyncWrite + Unpin>(
    guard: PublicationGuard,
    mut writer: W,
    bytes: &[u8],
) -> Result<(PublicationGuard, W), ObservationSinkFailure> {
    writer
        .write_all(bytes)
        .await
        .map_err(|_| ObservationSinkFailure("QUALIFICATION_WRITE_FAILED"))?;
    Ok((guard, writer))
}

async fn sync_stage<F>(
    guard: PublicationGuard,
    sync: F,
) -> Result<PublicationGuard, ObservationSinkFailure>
where
    F: Future<Output = std::io::Result<()>>,
{
    sync.await
        .map_err(|_| ObservationSinkFailure("QUALIFICATION_SYNC_FAILED"))?;
    Ok(guard)
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

    fn complete(mut self) {
        self.complete = true;
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
    if metadata.file_type().is_file() && (metadata.dev(), metadata.ino()) == identity {
        std::fs::remove_file(path).is_ok()
    } else {
        false
    }
}

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
