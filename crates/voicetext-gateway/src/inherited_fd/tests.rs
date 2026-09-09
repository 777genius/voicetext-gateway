//! Synthetic descriptor tests. No runtime, environment mutation, or provider access.
#![cfg(target_os = "linux")]
use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

fn fixture(bytes: &[u8]) -> (tempfile::TempDir, File) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret");
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    let file = File::open(path).unwrap();
    (dir, file)
}

#[test]
fn four_slots_unlinked_database_and_shared_offsets() {
    let mut dirs = Vec::new();
    let mut parents = Vec::new();
    let mut numbers = Vec::new();
    for slot in 0..4 {
        let (dir, mut parent) = fixture(b"synthetic-credential-000000000001\n");
        parent.seek(SeekFrom::Start(7)).unwrap();
        if slot == 1 {
            fs::remove_file(dir.path().join("secret")).unwrap();
        }
        numbers.push(parent.try_clone().unwrap().into_raw_fd());
        dirs.push(dir);
        parents.push(parent);
    }
    for _ in 0..4 {
        let config = GatewayConfig::from_lookup(|name| {
            if name == "VOICETEXT_SPOOL_DIR" {
                return Some("/synthetic-unused-spool".into());
            }
            voicetext_gateway::config::SECRET_FD_ENVS
                .iter()
                .position(|slot| *slot == name)
                .map(|slot| numbers[slot].to_string())
        })
        .unwrap();
        let captured = Captured::capture(&config).unwrap();
        for number in &numbers {
            assert_eq!(
                &**captured.read(*number).unwrap(),
                b"synthetic-credential-000000000001"
            );
            assert!(
                captured
                    .files
                    .iter()
                    .all(|(_, file, _)| file.as_raw_fd() > *numbers.iter().max().unwrap())
            );
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            captured
                .machine(&SecretSource::Descriptor(numbers[0]))
                .await
                .unwrap();
            for number in &numbers[1..] {
                assert_eq!(
                    captured
                        .text(&SecretSource::Descriptor(*number))
                        .await
                        .unwrap()
                        .expose_secret(),
                    "synthetic-credential-000000000001"
                );
            }
        });
        for parent in &mut parents {
            assert_eq!(parent.stream_position().unwrap(), 7);
        }
        drop(captured);
        numbers = parents
            .iter()
            .map(|p| p.try_clone().unwrap().into_raw_fd())
            .collect();
    }
    // Consume the final handoff copies as well.
    drop(linux_capture(&numbers).unwrap());
}

#[test]
fn rejects_modes_access_types_links_and_sizes() {
    for mode in [0o600, 0o440, 0o404, 0o000, 0o1400] {
        let (dir, file) = fixture(b"synthetic");
        fs::set_permissions(dir.path().join("secret"), fs::Permissions::from_mode(mode)).unwrap();
        assert!(linux_capture(&[file.into_raw_fd()]).is_err());
    }
    for size in [0, 16_385] {
        let (_dir, file) = fixture(&vec![b'x'; size]);
        assert!(linux_capture(&[file.into_raw_fd()]).is_err());
    }
    let (dir, file) = fixture(b"synthetic");
    fs::hard_link(dir.path().join("secret"), dir.path().join("alias")).unwrap();
    assert!(linux_capture(&[file.into_raw_fd()]).is_err());
    assert!(linux_capture(&[File::open(dir.path()).unwrap().into_raw_fd()]).is_err());
    let (dir, file) = fixture(b"synthetic");
    drop(file);
    let path = dir.path().join("secret");
    let opath = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(&path)
        .unwrap();
    assert!(linux_capture(&[opath.into_raw_fd()]).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    assert!(linux_capture(&[writer.into_raw_fd()]).is_err());
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(linux_capture(&[socket.into_raw_fd()]).is_err());
}

#[test]
fn complete_validation_precedes_duplicate_allocation() {
    let (_dir, file) = fixture(b"synthetic");
    let fd = file.into_raw_fd();
    assert!(linux_capture(&[fd, 2_000_000_000]).is_err());
    let (_dir, file) = fixture(b"synthetic");
    assert!(linux_capture(&[file.as_raw_fd(), file.as_raw_fd()]).is_err());
    // Duplicate syntax errors do not transfer ownership; the caller still owns this file.
    assert!(file.metadata().is_ok());
}

#[test]
fn mutation_and_malformed_content_are_rejected() {
    let (dir, file) = fixture(b"synthetic");
    let fd = file.into_raw_fd();
    let captured = linux_capture(&[fd]).unwrap();
    fs::set_permissions(dir.path().join("secret"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(dir.path().join("secret"), vec![b'x'; 16_385]).unwrap();
    assert!(captured.read(fd).is_err());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for bytes in [b"key\r\n".as_slice(), b"key\n\n", b"\xff", b"\0", b"\n"] {
        let (_dir, file) = fixture(bytes);
        let fd = file.into_raw_fd();
        let captured = linux_capture(&[fd]).unwrap();
        runtime.block_on(async {
            assert!(captured.text(&SecretSource::Descriptor(fd)).await.is_err());
            assert!(
                captured
                    .machine(&SecretSource::Descriptor(fd))
                    .await
                    .is_err()
            );
        });
    }
}

#[test]
fn custody_closure_in_isolated_process() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inherited_fd::tests::isolated_custody",
            "--ignored",
        ])
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "executed by custody_closure_in_isolated_process in a dedicated process"]
fn isolated_custody() {
    let exists = |fd| fs::symlink_metadata(format!("/proc/self/fd/{fd}")).is_ok();
    let (_dir, file) = fixture(b"synthetic");
    let original = file.into_raw_fd();
    let captured = linux_capture(&[original]).unwrap();
    assert!(!exists(original));
    let duplicate = captured.files[0].1.as_raw_fd();
    assert!(exists(duplicate));
    let replacement = File::open("/dev/null").unwrap();
    assert_eq!(replacement.as_raw_fd(), original);
    // A stale slot now names an invalid capability, never the previous secret.
    assert!(linux_capture(&[replacement.into_raw_fd()]).is_err());
    let replacement = File::open("/dev/null").unwrap();
    drop(captured);
    assert!(!exists(duplicate));
    assert!(replacement.metadata().is_ok()); // no retried close of the reused original
    drop(replacement);
    let (_dir, first) = fixture(b"synthetic");
    let (_other, last) = fixture(b"synthetic");
    let first = first.into_raw_fd();
    let last = last.into_raw_fd();
    assert!(linux_capture(&[first, 2_000_000_000, last]).is_err());
    assert!(!exists(first));
    assert!(!exists(last));
    let (_dir, first) = fixture(b"synthetic");
    let (dir, last) = fixture(b"synthetic");
    fs::set_permissions(dir.path().join("secret"), fs::Permissions::from_mode(0o600)).unwrap();
    let first = first.into_raw_fd();
    let last = last.into_raw_fd();
    assert!(linux_capture(&[first, last]).is_err());
    assert!(!exists(first));
    assert!(!exists(last));
}

#[test]
fn pipe_and_wrong_owner_are_rejected() {
    let (reader, _writer) = std::io::pipe().unwrap();
    assert!(linux_capture(&[reader.into_raw_fd()]).is_err());
    let (dir, file) = fixture(b"synthetic");
    // chown requires privilege; the owner mismatch case runs when this test has it.
    if std::os::unix::fs::chown(dir.path().join("secret"), Some(65534), None).is_ok() {
        assert!(linux_capture(&[file.into_raw_fd()]).is_err());
    }
}

#[test]
fn capture_metadata_remains_binding_until_read() {
    let (dir, file) = fixture(b"synthetic");
    let fd = file.into_raw_fd();
    let captured = linux_capture(&[fd]).unwrap();
    fs::set_permissions(dir.path().join("secret"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(captured.read(fd).is_err());
}

#[tokio::test]
async fn proc_fd_named_file_remains_a_rejected_symlink() {
    let (_dir, file) = fixture(b"synthetic-credential-000000000001");
    let path = format!("/proc/self/fd/{}", file.as_raw_fd());
    assert!(matches!(
        MachineSecret::read_from_file(path).await,
        Err(voicetext_gateway::secret::SecretFileError::Symlink)
    ));
}

#[test]
fn partial_duplication_failure_closes_all_owned_handles() {
    let status = std::process::Command::new("sh")
        .args(["-c", "ulimit -n 64 && exec \"$1\" --exact inherited_fd::tests::isolated_duplication_failure --ignored", "fd-custody"])
        .arg(std::env::current_exe().unwrap()).status().unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "executed with a bounded descriptor limit by its parent test"]
fn isolated_duplication_failure() {
    let (_dir, first) = fixture(b"synthetic");
    let (_other, second) = fixture(b"synthetic");
    let first = first.into_raw_fd();
    let second = second.into_raw_fd();
    let mut fillers = Vec::new();
    for _ in 0..64 {
        let Ok(file) = File::open("/dev/null") else {
            break;
        };
        fillers.push(file);
    }
    assert!(fillers.len() < 64);
    assert!(!fillers.is_empty());
    let available = fillers.last().unwrap().as_raw_fd();
    assert!(available > first.max(second));
    drop(fillers.pop()); // enough space for the first duplicate, but not the second
    assert!(linux_capture(&[first, second]).is_err());
    for fd in [first, second, available] {
        assert!(fs::symlink_metadata(format!("/proc/self/fd/{fd}")).is_err());
    }
}
