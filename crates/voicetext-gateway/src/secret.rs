//! Redacted machine-token secrets loaded from mounted files.

use std::fmt;
use std::path::Path;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::io::AsyncReadExt;

/// Maximum accepted size of a mounted secret file, including its final newline.
pub const MAX_SECRET_FILE_BYTES: usize = 16 * 1024;
/// Minimum entropy-bearing byte length required for a gateway service token.
pub const MIN_MACHINE_TOKEN_BYTES: usize = 32;
const MAX_SECRET_READ_BYTES: u64 = 16 * 1024 + 1;

/// A one-way representation of a machine authentication token.
///
/// The plaintext is discarded during construction. This type intentionally does
/// not implement `Clone`, serialization, or any plaintext accessor.
pub struct MachineSecret {
    digest: [u8; 32],
}

impl MachineSecret {
    /// Builds a secret from one visible-ASCII token.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::TooShort`] for a token shorter than 32 bytes, or
    /// [`SecretError::InvalidCharacter`] for whitespace, control, or non-ASCII bytes.
    pub fn from_token(token: &[u8]) -> Result<Self, SecretError> {
        validate_token(token)?;
        Ok(Self {
            digest: Sha256::digest(token).into(),
        })
    }

    /// Reads and validates a token from a mounted secret file.
    ///
    /// Exactly one terminal LF is removed. All other whitespace and control bytes
    /// are rejected, so an extra newline or CRLF cannot silently change a token.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the path is a symlink, is not a regular file,
    /// has unsafe Unix write permissions, is too large, changes while opening, or
    /// contains an invalid token. Error values never contain secret bytes.
    pub async fn read_from_file(path: impl AsRef<Path>) -> Result<Self, SecretFileError> {
        let mut bytes = read_secret_bytes(path.as_ref()).await?;
        let result = Self::from_token(&bytes).map_err(SecretFileError::InvalidToken);
        bytes.fill(0);
        result
    }

    /// Compares a candidate without a length-dependent equality branch.
    pub(crate) fn verifies(&self, candidate: &[u8]) -> bool {
        let candidate_digest: [u8; 32] = Sha256::digest(candidate).into();
        bool::from(self.digest.ct_eq(&candidate_digest))
    }
}

/// Redacted secret text for credentials that must be passed to an external client.
///
/// Unlike [`MachineSecret`], this type retains plaintext because database and
/// provider clients require it. It is neither cloneable nor serializable and
/// overwrites its allocation when dropped.
pub struct SecretText {
    bytes: Vec<u8>,
}

impl SecretText {
    /// Copies validated UTF-8 secret text into an owned, redacted container.
    ///
    /// # Errors
    ///
    /// Returns [`SecretTextError::Empty`] for empty text or
    /// [`SecretTextError::InvalidCharacter`] for control/newline characters.
    pub fn from_text(text: &str) -> Result<Self, SecretTextError> {
        validate_secret_text(text)?;
        Ok(Self {
            bytes: text.as_bytes().to_vec(),
        })
    }

    /// Reads secret text from a hardened mounted file.
    ///
    /// This uses the same size, symlink, regular-file, permission, and file-swap
    /// defenses as [`MachineSecret::read_from_file`]. Exactly one terminal LF is
    /// removed before validating UTF-8 and control characters.
    ///
    /// # Errors
    ///
    /// Returns a typed, non-secret-bearing error when file hardening, UTF-8, or
    /// content validation fails.
    pub async fn read_from_file(path: impl AsRef<Path>) -> Result<Self, SecretFileError> {
        let bytes = read_secret_bytes(path.as_ref()).await?;
        Self::from_owned_bytes(bytes).map_err(SecretFileError::InvalidText)
    }

    /// Exposes plaintext only at the final composition boundary that needs it.
    ///
    /// Callers must not log, serialize, persist, or include this value in errors.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        std::str::from_utf8(&self.bytes).unwrap_or_default()
    }

    fn from_owned_bytes(mut bytes: Vec<u8>) -> Result<Self, SecretTextError> {
        let validation = match std::str::from_utf8(&bytes) {
            Ok(text) => validate_secret_text(text),
            Err(_) => Err(SecretTextError::InvalidUtf8),
        };
        if let Err(error) = validation {
            bytes.fill(0);
            return Err(error);
        }
        Ok(Self { bytes })
    }

    fn erase(&mut self) {
        self.bytes.fill(0);
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        self.erase();
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretText(REDACTED)")
    }
}

impl fmt::Display for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl fmt::Debug for MachineSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MachineSecret(REDACTED)")
    }
}

impl fmt::Display for MachineSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// Validation failure for plaintext token input.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum SecretError {
    /// The token does not meet the service-token minimum length.
    #[error("machine token must be at least {minimum} bytes")]
    TooShort {
        /// Required minimum, without disclosing the supplied length or value.
        minimum: usize,
    },
    /// The token is not one unambiguous visible-ASCII value.
    #[error("machine token contains whitespace, control, or non-ASCII bytes")]
    InvalidCharacter,
}

/// Validation failure for retained secret text.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum SecretTextError {
    /// The text has no bytes.
    #[error("secret text must not be empty")]
    Empty,
    /// Mounted bytes are not valid UTF-8.
    #[error("secret text must be valid UTF-8")]
    InvalidUtf8,
    /// Control and newline characters are forbidden.
    #[error("secret text contains control or newline characters")]
    InvalidCharacter,
}

/// Safe failure while loading a mounted secret.
#[derive(Debug, Error)]
pub enum SecretFileError {
    /// Metadata could not be inspected.
    #[error("could not inspect secret file metadata")]
    Metadata(#[source] std::io::Error),
    /// The file could not be opened.
    #[error("could not open secret file")]
    Open(#[source] std::io::Error),
    /// The file could not be read.
    #[error("could not read secret file")]
    Read(#[source] std::io::Error),
    /// Mounted secrets must be ordinary files.
    #[error("secret path must be a regular file")]
    NotRegularFile,
    /// Symbolic links are rejected rather than followed.
    #[error("secret path must not be a symbolic link")]
    Symlink,
    /// The file changed between inspection and opening.
    #[error("secret file changed while it was being opened")]
    ChangedDuringOpen,
    /// Unix group/world write bits would permit unauthorized replacement.
    #[error("secret file must not be group-writable or world-writable")]
    InsecurePermissions,
    /// The bounded loader refused oversized input.
    #[error("secret file exceeds the maximum size of {maximum} bytes")]
    TooLarge {
        /// Maximum accepted raw file length.
        maximum: usize,
    },
    /// The file contents are not a valid token.
    #[error("secret file does not contain a valid machine token")]
    InvalidToken(#[source] SecretError),
    /// The file contents are not valid retained secret text.
    #[error("secret file does not contain valid secret text")]
    InvalidText(#[source] SecretTextError),
}

fn validate_token(token: &[u8]) -> Result<(), SecretError> {
    if token.len() < MIN_MACHINE_TOKEN_BYTES {
        return Err(SecretError::TooShort {
            minimum: MIN_MACHINE_TOKEN_BYTES,
        });
    }
    if token.iter().any(|byte| !(0x21..=0x7e).contains(byte)) {
        return Err(SecretError::InvalidCharacter);
    }
    Ok(())
}

fn validate_secret_text(text: &str) -> Result<(), SecretTextError> {
    if text.is_empty() {
        return Err(SecretTextError::Empty);
    }
    if text
        .chars()
        .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
    {
        return Err(SecretTextError::InvalidCharacter);
    }
    Ok(())
}

async fn read_secret_bytes(path: &Path) -> Result<Vec<u8>, SecretFileError> {
    let before = tokio::fs::symlink_metadata(path)
        .await
        .map_err(SecretFileError::Metadata)?;
    validate_metadata(&before)?;

    let file = tokio::fs::File::open(path)
        .await
        .map_err(SecretFileError::Open)?;
    let opened = file.metadata().await.map_err(SecretFileError::Metadata)?;
    validate_metadata(&opened)?;
    ensure_same_file(&before, &opened)?;

    let expected_length = usize::try_from(opened.len()).unwrap_or(MAX_SECRET_FILE_BYTES);
    let mut bytes = Vec::with_capacity(MAX_SECRET_FILE_BYTES.min(expected_length));
    file.take(MAX_SECRET_READ_BYTES)
        .read_to_end(&mut bytes)
        .await
        .map_err(SecretFileError::Read)?;
    if bytes.len() > MAX_SECRET_FILE_BYTES {
        bytes.fill(0);
        return Err(SecretFileError::TooLarge {
            maximum: MAX_SECRET_FILE_BYTES,
        });
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok(bytes)
}

fn validate_metadata(metadata: &std::fs::Metadata) -> Result<(), SecretFileError> {
    if metadata.file_type().is_symlink() {
        return Err(SecretFileError::Symlink);
    }
    if !metadata.is_file() {
        return Err(SecretFileError::NotRegularFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(SecretFileError::InsecurePermissions);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_same_file(
    before: &std::fs::Metadata,
    opened: &std::fs::Metadata,
) -> Result<(), SecretFileError> {
    use std::os::unix::fs::MetadataExt;
    if before.dev() != opened.dev() || before.ino() != opened.ino() {
        return Err(SecretFileError::ChangedDuringOpen);
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_file(
    before: &std::fs::Metadata,
    opened: &std::fs::Metadata,
) -> Result<(), SecretFileError> {
    if before.len() != opened.len() || before.modified().ok() != opened.modified().ok() {
        return Err(SecretFileError::ChangedDuringOpen);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn secret_is_always_redacted() {
        let secret = MachineSecret::from_token(b"do-not-print-this-service-token-0001").unwrap();
        assert_eq!(format!("{secret}"), "[REDACTED]");
        assert_eq!(format!("{secret:?}"), "MachineSecret(REDACTED)");
    }

    #[test]
    fn retained_secret_is_redacted_and_can_be_explicitly_exposed() {
        let secret = SecretText::from_text("postgres://user:p@ss/db?sslmode=require").unwrap();
        assert_eq!(
            secret.expose_secret(),
            "postgres://user:p@ss/db?sslmode=require"
        );
        assert_eq!(format!("{secret}"), "[REDACTED]");
        assert_eq!(format!("{secret:?}"), "SecretText(REDACTED)");
    }

    #[test]
    fn retained_secret_erases_its_owned_allocation() {
        let mut secret = SecretText::from_text("provider-key").unwrap();
        secret.erase();
        assert!(secret.bytes.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn retained_secret_rejects_empty_invalid_utf8_and_controls() {
        assert!(matches!(
            SecretText::from_text(""),
            Err(SecretTextError::Empty)
        ));
        assert!(matches!(
            SecretText::from_text("key\nvalue"),
            Err(SecretTextError::InvalidCharacter)
        ));
        assert!(matches!(
            SecretText::from_text("key\u{2028}value"),
            Err(SecretTextError::InvalidCharacter)
        ));
        assert!(matches!(
            SecretText::from_owned_bytes(vec![0xff]),
            Err(SecretTextError::InvalidUtf8)
        ));
    }

    #[tokio::test]
    async fn retained_secret_loader_uses_hardened_file_rules() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("database-url");
        fs::write(&path, "postgres://u:p@host/db\n").unwrap();
        secure_permissions(&path);

        let secret = SecretText::read_from_file(&path).await.unwrap();
        assert_eq!(secret.expose_secret(), "postgres://u:p@host/db");

        fs::write(&path, b"key\0suffix").unwrap();
        assert!(matches!(
            SecretText::read_from_file(path).await,
            Err(SecretFileError::InvalidText(
                SecretTextError::InvalidCharacter
            ))
        ));
    }

    #[test]
    fn verification_handles_equal_and_different_lengths() {
        let secret = MachineSecret::from_token(b"correct-service-token-000000000001").unwrap();
        assert!(secret.verifies(b"correct-service-token-000000000001"));
        assert!(!secret.verifies(b"wrong"));
        assert!(!secret.verifies(b"correct-service-token-000000000001-with-suffix"));
    }

    #[test]
    fn machine_token_enforces_secret_safe_thirty_two_byte_boundary() {
        let error = MachineSecret::from_token(&[b'x'; MIN_MACHINE_TOKEN_BYTES - 1])
            .err()
            .unwrap();
        assert_eq!(
            error,
            SecretError::TooShort {
                minimum: MIN_MACHINE_TOKEN_BYTES
            }
        );
        assert!(!format!("{error:?}").contains("31"));
        assert!(MachineSecret::from_token(&[b'x'; MIN_MACHINE_TOKEN_BYTES]).is_ok());
    }

    #[tokio::test]
    async fn mounted_machine_token_enforces_the_same_boundary() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("service-token");
        fs::write(&path, [b'x'; MIN_MACHINE_TOKEN_BYTES - 1]).unwrap();
        secure_permissions(&path);
        assert!(matches!(
            MachineSecret::read_from_file(&path).await,
            Err(SecretFileError::InvalidToken(SecretError::TooShort {
                minimum: MIN_MACHINE_TOKEN_BYTES
            }))
        ));
        fs::write(&path, [b'x'; MIN_MACHINE_TOKEN_BYTES]).unwrap();
        let secret = MachineSecret::read_from_file(&path).await.unwrap();
        assert!(secret.verifies(&[b'x'; MIN_MACHINE_TOKEN_BYTES]));
    }

    #[tokio::test]
    async fn loader_trims_one_terminal_lf() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("token");
        fs::write(&path, b"mounted-service-token-000000000001\n").unwrap();
        secure_permissions(&path);

        let secret = MachineSecret::read_from_file(&path).await.unwrap();
        assert!(secret.verifies(b"mounted-service-token-000000000001"));
    }

    #[tokio::test]
    async fn loader_rejects_ambiguous_or_control_input() {
        let directory = tempdir().unwrap();
        for (name, bytes) in [
            ("empty", b"".as_slice()),
            ("extra-newline", b"token\n\n".as_slice()),
            ("crlf", b"token\r\n".as_slice()),
            ("nul", b"token\0suffix".as_slice()),
            ("space", b"token suffix".as_slice()),
        ] {
            let path = directory.path().join(name);
            fs::write(&path, bytes).unwrap();
            secure_permissions(&path);
            assert!(matches!(
                MachineSecret::read_from_file(path).await,
                Err(SecretFileError::InvalidToken(_))
            ));
        }
    }

    #[tokio::test]
    async fn loader_rejects_oversized_files() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("large");
        fs::write(&path, vec![b'x'; MAX_SECRET_FILE_BYTES + 1]).unwrap();
        secure_permissions(&path);
        assert!(matches!(
            MachineSecret::read_from_file(path).await,
            Err(SecretFileError::TooLarge { .. })
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn loader_rejects_symlink_and_unsafe_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().unwrap();
        let target = directory.path().join("target");
        fs::write(&target, b"token").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        let link = directory.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(matches!(
            MachineSecret::read_from_file(link).await,
            Err(SecretFileError::Symlink)
        ));

        fs::set_permissions(&target, fs::Permissions::from_mode(0o620)).unwrap();
        assert!(matches!(
            MachineSecret::read_from_file(target).await,
            Err(SecretFileError::InsecurePermissions)
        ));
    }

    fn secure_permissions(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
