//! Bounded, content-addressed filesystem storage for accepted batch audio.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use uuid::Uuid;
use voicetext_speech::application::ports::{
    BatchAudioHandle, BatchAudioSpool, BatchAudioSpoolFailure, BatchAudioStoreOutcome, BatchJobId,
    BoxFuture,
};

/// Durable content-addressed spool rooted in one canonical, non-symlink directory.
#[derive(Clone, Debug)]
pub struct DurableFileSpool {
    root: PathBuf,
    maximum_bytes: usize,
}

impl DurableFileSpool {
    /// Opens an existing spool directory and pins its canonical location.
    ///
    /// # Errors
    ///
    /// Rejects zero capacity, symlink roots, and roots that are missing or not directories.
    pub fn new(
        root: impl AsRef<Path>,
        maximum_bytes: usize,
    ) -> Result<Self, BatchAudioSpoolFailure> {
        if maximum_bytes == 0 {
            return Err(BatchAudioSpoolFailure::CapacityExceeded);
        }
        let root = root.as_ref();
        let metadata = fs::symlink_metadata(root).map_err(unavailable)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid_storage());
        }
        let canonical = fs::canonicalize(root).map_err(unavailable)?;
        Ok(Self {
            root: canonical,
            maximum_bytes,
        })
    }

    fn store_blocking(
        &self,
        id: &BatchJobId,
        audio: &[u8],
    ) -> Result<BatchAudioStoreOutcome, BatchAudioSpoolFailure> {
        if audio.is_empty() || audio.len() > self.maximum_bytes {
            return Err(BatchAudioSpoolFailure::CapacityExceeded);
        }
        let job_id = canonical_job_id(id)?;
        let digest = hex::encode(Sha256::digest(audio));
        let handle = format!("{job_id}-{digest}.ogg");
        let final_path = self.root.join(&handle);
        let created = create_file_atomic(&self.root, &final_path, audio)?;
        if created {
            sync_directory(&self.root)?;
        } else {
            verify_file(&final_path, &digest, self.maximum_bytes)?;
        }

        let claim_path = self.root.join(format!("{job_id}.identity"));
        match create_file_atomic(&self.root, &claim_path, digest.as_bytes()) {
            Ok(true) => sync_directory(&self.root)?,
            Ok(false) => {
                let claimed = read_small_regular_file(&claim_path, 64)?;
                if claimed != digest.as_bytes() {
                    if created {
                        let _ = fs::remove_file(&final_path);
                    }
                    return Err(BatchAudioSpoolFailure::IdentityConflict);
                }
            }
            Err(error) => {
                return Err(error);
            }
        }

        let handle = BatchAudioHandle::new(handle);
        Ok(if created {
            BatchAudioStoreOutcome::Stored(handle)
        } else {
            BatchAudioStoreOutcome::Existing(handle)
        })
    }

    fn read_blocking(&self, handle: &BatchAudioHandle) -> Result<Vec<u8>, BatchAudioSpoolFailure> {
        let parsed = parse_handle(handle.as_str())?;
        let path = self.root.join(handle.as_str());
        read_verified_file(&path, parsed.digest, self.maximum_bytes)
    }

    fn remove_blocking(&self, handle: &BatchAudioHandle) -> Result<(), BatchAudioSpoolFailure> {
        let parsed = parse_handle(handle.as_str())?;
        let path = self.root.join(handle.as_str());
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(invalid_storage());
            }
            Ok(_) => fs::remove_file(&path).map_err(unavailable)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(unavailable(error)),
        }

        let claim = self.root.join(format!("{}.identity", parsed.job_id));
        match read_small_regular_file(&claim, 64) {
            Ok(value) if value == parsed.digest.as_bytes() => {
                fs::remove_file(&claim).map_err(unavailable)?;
            }
            Ok(_) | Err(BatchAudioSpoolFailure::Missing) => {}
            Err(error) => return Err(error),
        }
        sync_directory(&self.root)
    }
}

impl BatchAudioSpool for DurableFileSpool {
    fn store(
        &self,
        id: BatchJobId,
        audio: Vec<u8>,
    ) -> BoxFuture<'_, Result<BatchAudioStoreOutcome, BatchAudioSpoolFailure>> {
        let spool = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || spool.store_blocking(&id, &audio))
                .await
                .map_err(join_failure)?
        })
    }

    fn read<'a>(
        &'a self,
        handle: &'a BatchAudioHandle,
    ) -> BoxFuture<'a, Result<Vec<u8>, BatchAudioSpoolFailure>> {
        let spool = self.clone();
        let handle = handle.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || spool.read_blocking(&handle))
                .await
                .map_err(join_failure)?
        })
    }

    fn remove<'a>(
        &'a self,
        handle: &'a BatchAudioHandle,
    ) -> BoxFuture<'a, Result<(), BatchAudioSpoolFailure>> {
        let spool = self.clone();
        let handle = handle.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || spool.remove_blocking(&handle))
                .await
                .map_err(join_failure)?
        })
    }
}

struct ParsedHandle<'a> {
    job_id: &'a str,
    digest: &'a str,
}

fn canonical_job_id(id: &BatchJobId) -> Result<String, BatchAudioSpoolFailure> {
    let parsed =
        Uuid::parse_str(id.as_str()).map_err(|_| BatchAudioSpoolFailure::IdentityConflict)?;
    let canonical = parsed.hyphenated().to_string();
    if canonical != id.as_str() {
        return Err(BatchAudioSpoolFailure::IdentityConflict);
    }
    Ok(canonical)
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the opaque handle grammar requires lowercase .ogg exactly"
)]
fn parse_handle(value: &str) -> Result<ParsedHandle<'_>, BatchAudioSpoolFailure> {
    const HANDLE_BYTES: usize = 36 + 1 + 64 + 4;
    if value.len() != HANDLE_BYTES || !value.is_ascii() || !value.ends_with(".ogg") {
        return Err(invalid_storage());
    }
    let job_id = &value[..36];
    let digest = &value[37..101];
    if value.as_bytes()[36] != b'-'
        || !Uuid::parse_str(job_id).is_ok_and(|uuid| uuid.hyphenated().to_string() == job_id)
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_storage());
    }
    Ok(ParsedHandle { job_id, digest })
}

fn create_file_atomic(
    root: &Path,
    final_path: &Path,
    contents: &[u8],
) -> Result<bool, BatchAudioSpoolFailure> {
    let temporary = root.join(format!(".{}.tmp", Uuid::new_v4().hyphenated()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(unavailable)?;
    let result = (|| {
        file.write_all(contents).map_err(unavailable)?;
        file.sync_all().map_err(unavailable)?;
        match fs::hard_link(&temporary, final_path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(unavailable(error)),
        }
    })();
    drop(file);
    let cleanup = fs::remove_file(&temporary);
    if let Err(error) = cleanup
        && result.is_ok()
    {
        return Err(unavailable(error));
    }
    result
}

fn verify_file(
    path: &Path,
    expected_digest: &str,
    maximum_bytes: usize,
) -> Result<(), BatchAudioSpoolFailure> {
    read_verified_file(path, expected_digest, maximum_bytes).map(drop)
}

fn read_verified_file(
    path: &Path,
    expected_digest: &str,
    maximum_bytes: usize,
) -> Result<Vec<u8>, BatchAudioSpoolFailure> {
    let metadata = regular_file_metadata(path)?;
    let maximum = u64::try_from(maximum_bytes).map_err(|_| invalid_storage())?;
    if metadata.len() == 0 || metadata.len() > maximum {
        return Err(if metadata.len() > maximum {
            BatchAudioSpoolFailure::CapacityExceeded
        } else {
            invalid_storage()
        });
    }
    let mut file = File::open(path).map_err(unavailable)?;
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.len()).map_err(|_| invalid_storage())?);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(unavailable)?;
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > maximum_bytes {
            return Err(BatchAudioSpoolFailure::CapacityExceeded);
        }
        digest.update(&buffer[..read]);
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.is_empty() || hex::encode(digest.finalize()) != expected_digest {
        return Err(invalid_storage());
    }
    Ok(bytes)
}

fn read_small_regular_file(
    path: &Path,
    maximum_bytes: usize,
) -> Result<Vec<u8>, BatchAudioSpoolFailure> {
    let metadata = regular_file_metadata(path)?;
    let maximum = u64::try_from(maximum_bytes).map_err(|_| invalid_storage())?;
    if metadata.len() == 0 || metadata.len() > maximum {
        return Err(if metadata.len() > maximum {
            BatchAudioSpoolFailure::CapacityExceeded
        } else {
            invalid_storage()
        });
    }
    let file = File::open(path).map_err(unavailable)?;
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.len()).map_err(|_| invalid_storage())?);
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    if bytes.len() > maximum_bytes {
        return Err(BatchAudioSpoolFailure::CapacityExceeded);
    }
    Ok(bytes)
}

fn regular_file_metadata(path: &Path) -> Result<fs::Metadata, BatchAudioSpoolFailure> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BatchAudioSpoolFailure::Missing
        } else {
            unavailable(error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid_storage());
    }
    Ok(metadata)
}

fn sync_directory(root: &Path) -> Result<(), BatchAudioSpoolFailure> {
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(unavailable)
}

fn invalid_storage() -> BatchAudioSpoolFailure {
    BatchAudioSpoolFailure::Unavailable {
        code: "INVALID_SPOOL_ARTIFACT".into(),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "I/O Result::map_err transfers ownership"
)]
fn unavailable(error: std::io::Error) -> BatchAudioSpoolFailure {
    BatchAudioSpoolFailure::Unavailable {
        code: format!("SPOOL_IO_{:?}", error.kind()).to_ascii_uppercase(),
    }
}

fn join_failure(_: tokio::task::JoinError) -> BatchAudioSpoolFailure {
    BatchAudioSpoolFailure::Unavailable {
        code: "SPOOL_TASK_FAILED".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;

    use super::*;

    fn id() -> BatchJobId {
        BatchJobId::new("123e4567-e89b-12d3-a456-426614174000")
    }

    fn spool(root: &Path) -> DurableFileSpool {
        DurableFileSpool::new(root, 16).unwrap()
    }

    #[tokio::test]
    async fn exact_replay_preserves_cleanup_ownership() {
        let directory = tempdir().unwrap();
        let spool = spool(directory.path());
        let stored = spool.store(id(), b"audio".to_vec()).await.unwrap();
        let replay = spool.store(id(), b"audio".to_vec()).await.unwrap();
        let BatchAudioStoreOutcome::Stored(handle) = stored else {
            panic!()
        };
        assert_eq!(replay, BatchAudioStoreOutcome::Existing(handle.clone()));
        assert_eq!(spool.read(&handle).await.unwrap(), b"audio");
        spool.remove(&handle).await.unwrap();
        spool.remove(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_conflicting_content_and_invalid_input() {
        let directory = tempdir().unwrap();
        let spool = spool(directory.path());
        spool.store(id(), b"first".to_vec()).await.unwrap();
        assert_eq!(
            spool.store(id(), b"other".to_vec()).await,
            Err(BatchAudioSpoolFailure::IdentityConflict)
        );
        assert_eq!(
            spool.store(id(), Vec::new()).await,
            Err(BatchAudioSpoolFailure::CapacityExceeded)
        );
        assert_eq!(
            spool.store(id(), vec![0; 17]).await,
            Err(BatchAudioSpoolFailure::CapacityExceeded)
        );
        assert_eq!(
            spool.read(&BatchAudioHandle::new("../escape.ogg")).await,
            Err(invalid_storage())
        );
    }

    #[tokio::test]
    async fn detects_tampering() {
        let directory = tempdir().unwrap();
        let spool = spool(directory.path());
        let BatchAudioStoreOutcome::Stored(handle) =
            spool.store(id(), b"audio".to_vec()).await.unwrap()
        else {
            panic!()
        };
        fs::write(directory.path().join(handle.as_str()), b"wrong").unwrap();
        assert_eq!(spool.read(&handle).await, Err(invalid_storage()));
    }

    #[tokio::test]
    async fn concurrent_same_content_has_one_cleanup_owner() {
        let directory = tempdir().unwrap();
        let spool = Arc::new(spool(directory.path()));
        let left = tokio::spawn({
            let spool = Arc::clone(&spool);
            async move { spool.store(id(), b"audio".to_vec()).await.unwrap() }
        });
        let right = tokio::spawn({
            let spool = Arc::clone(&spool);
            async move { spool.store(id(), b"audio".to_vec()).await.unwrap() }
        });
        let outcomes = [left.await.unwrap(), right.await.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, BatchAudioStoreOutcome::Stored(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, BatchAudioStoreOutcome::Existing(_)))
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn constructor_rejects_symlink_root() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::create_dir(&target).unwrap();
        symlink(target, &link).unwrap();
        assert!(matches!(
            DurableFileSpool::new(link, 16),
            Err(BatchAudioSpoolFailure::Unavailable { code }) if code == "INVALID_SPOOL_ARTIFACT"
        ));
    }
}
