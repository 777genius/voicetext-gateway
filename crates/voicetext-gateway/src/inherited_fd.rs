//! Linux launcher handoff, captured before runtime/thread/I/O initialization.
//! Slot provenance comes from the authenticated launcher, not an inode tuple.

use voicetext_gateway::config::{GatewayConfig, SecretSource};
use voicetext_gateway::secret::{MachineSecret, SecretText};
use zeroize::Zeroizing;

#[derive(Debug, Default)]
pub(crate) struct Captured {
    #[cfg(target_os = "linux")]
    files: Vec<(i32, std::fs::File, std::fs::Metadata)>,
}

impl Captured {
    // Only main calls this, before creating threads or opening any startup resources.
    pub(crate) fn capture(config: &GatewayConfig) -> Result<Self, ()> {
        let sources = [
            Some(&config.bearer_token_file),
            Some(&config.postgres_url_file),
            config.deepgram_api_key_file.as_ref(),
            config.elevenlabs_api_key_file.as_ref(),
        ];
        let numbers: Vec<_> = sources
            .into_iter()
            .flatten()
            .filter_map(|source| match source {
                SecretSource::Descriptor(fd) => Some(*fd),
                SecretSource::File(_) => None,
            })
            .collect();
        #[cfg(target_os = "linux")]
        {
            linux_capture(&numbers)
        }
        #[cfg(not(target_os = "linux"))]
        {
            if numbers.is_empty() {
                Ok(Self::default())
            } else {
                Err(())
            }
        }
    }

    pub(crate) async fn machine(&self, source: &SecretSource) -> Result<MachineSecret, ()> {
        match source {
            SecretSource::File(path) => MachineSecret::read_from_file(path).await.map_err(|_| ()),
            SecretSource::Descriptor(fd) => {
                MachineSecret::from_token(&self.read(*fd)?).map_err(|_| ())
            }
        }
    }

    pub(crate) async fn text(&self, source: &SecretSource) -> Result<SecretText, ()> {
        match source {
            SecretSource::File(path) => SecretText::read_from_file(path).await.map_err(|_| ()),
            SecretSource::Descriptor(fd) => {
                let bytes = self.read(*fd)?;
                SecretText::from_text(std::str::from_utf8(&bytes).map_err(|_| ())?).map_err(|_| ())
            }
        }
    }

    fn read(&self, number: i32) -> Result<Zeroizing<Vec<u8>>, ()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::FileExt;
            let entry = self
                .files
                .iter()
                .find(|(fd, _, _)| *fd == number)
                .ok_or(())?;
            let file = &entry.1;
            let before = file.metadata().map_err(|_| ())?;
            if identity(&before) != identity(&entry.2) {
                return Err(());
            }
            let length = usize::try_from(before.len()).map_err(|_| ())?;
            if !(1..=16_384).contains(&length) {
                return Err(());
            }
            let mut bytes = Zeroizing::new(vec![0; length + 1]);
            let mut read = 0;
            loop {
                match file.read_at(&mut bytes[read..], read as u64) {
                    Ok(0) => break,
                    Ok(count) => {
                        read += count;
                        if read == bytes.len() {
                            return Err(());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return Err(()),
                }
            }
            let after = file.metadata().map_err(|_| ())?;
            if read != length || identity(&before) != identity(&after) {
                return Err(());
            }
            bytes.truncate(read);
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            Ok(bytes)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = number;
            Err(())
        }
    }
}

#[cfg(target_os = "linux")]
fn identity(m: &std::fs::Metadata) -> (u64, u64, u64, u64, u32, u32, i64, i64, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (
        m.dev(),
        m.ino(),
        m.len(),
        m.nlink(),
        m.uid(),
        m.mode(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}

// Audited exception: libc accepts raw, possibly closed input numbers, whereas borrowing an
// invalid BorrowedFd would violate Rust's safety contract. No raw descriptor escapes here.
// Caller is the single-threaded process entrypoint; numbers are transferred by the launcher.
// F_GETFD proves validity before ownership transfer. Owned Files close once, without retry.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn linux_capture(numbers: &[i32]) -> Result<Captured, ()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::MetadataExt;
    let highest = numbers.iter().copied().max().unwrap_or(2);
    if numbers.iter().any(|fd| *fd < 3 || *fd == i32::MAX)
        || numbers
            .iter()
            .enumerate()
            .any(|(i, fd)| numbers[..i].contains(fd))
    {
        return Err(());
    }
    let mut originals = Vec::new();
    let mut invalid = false;
    for &fd in numbers {
        // SAFETY: F_GETFD accepts any integer and has no pointer argument.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            invalid = true;
            continue;
        }
        // SAFETY: validated open fd, unique in this set, launcher transferred ownership;
        // no threads/runtime or other owner in this process may close it concurrently.
        originals.push(unsafe { std::fs::File::from_raw_fd(fd) });
    }
    if invalid {
        return Err(());
    }
    let mut metadata = Vec::new();
    for file in &originals {
        let m = file.metadata().map_err(|_| ())?;
        // SAFETY: live owned fd; F_GETFL takes no pointer. geteuid has no arguments.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        let uid = unsafe { libc::geteuid() };
        if flags < 0
            || flags & libc::O_ACCMODE != libc::O_RDONLY
            || flags & libc::O_PATH != 0
            || !m.is_file()
            || m.uid() != uid
            || m.mode() & 0o7777 != 0o400
            || m.nlink() > 1
            || !(1..=16_384).contains(&m.len())
        {
            return Err(());
        }
        metadata.push(m);
    }
    let mut captured = Captured::default();
    for (file, before) in originals.iter().zip(metadata) {
        // SAFETY: live owned fd and checked minimum. Every input was validated first;
        // duplicates are above ALL input numbers, including any closed malicious slot.
        let duplicate =
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, highest + 1) };
        if duplicate < 0 {
            return Err(());
        }
        // SAFETY: successful fcntl returned a fresh descriptor exclusively owned here.
        let duplicate = unsafe { std::fs::File::from_raw_fd(duplicate) };
        if identity(&before) != identity(&duplicate.metadata().map_err(|_| ())?) {
            return Err(());
        }
        captured.files.push((file.as_raw_fd(), duplicate, before));
    }
    // All originals drop exactly once here; duplicates remain owned until composition ends.
    Ok(captured)
}

#[cfg(test)]
mod tests;
