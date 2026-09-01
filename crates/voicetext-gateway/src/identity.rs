//! Stable request identities shared by the compatibility HTTP boundary and storage.

use std::cmp::Ordering;
use std::fmt;

use sha2::{Digest, Sha256};
use uuid::Uuid;
use voicetext_speech::domain::batch::{BatchProfile, BatchRequestFingerprint};

const IDEMPOTENCY_KEY_BYTES: usize = 64;
const MAX_IDENTITY_BYTES: usize = 256;
const MAX_KEYTERMS: usize = 100;
const MAX_KEYTERM_BYTES: usize = 200;
const MAX_AUDIO_BYTES: usize = 64 * 1024 * 1024;

/// Public namespace for deterministic `VoiceText` batch job identifiers.
///
/// This is `UUIDv5`(`NAMESPACE_URL`,
/// `https://github.com/777genius/voicetext-gateway/jobs/v1`). It is frozen as part of the
/// compatibility contract; changing it would give the same idempotency key a different job ID.
pub const JOB_ID_NAMESPACE: Uuid = Uuid::from_u128(0xa7ef_59d0_400d_5b12_ac64_32fb_d576_fbbc);

/// A canonical lowercase SHA-256 hex idempotency key accepted by the HTTP boundary.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(Box<str>);

impl IdempotencyKey {
    /// Parses the exact `X-Idempotency-Key` representation used by `VoiceText` clients.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidIdempotencyKey`] unless `value` is exactly 64 lowercase hexadecimal
    /// ASCII bytes. Uppercase keys are rejected instead of silently changing request identity.
    pub fn parse(value: &str) -> Result<Self, InvalidIdempotencyKey> {
        if value.len() != IDEMPOTENCY_KEY_BYTES {
            return Err(InvalidIdempotencyKey::InvalidLength);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(InvalidIdempotencyKey::NotLowercaseHex);
        }
        Ok(Self(value.into()))
    }

    /// Returns the canonical wire representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for IdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IdempotencyKey([REDACTED])")
    }
}

/// Why an `X-Idempotency-Key` value was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidIdempotencyKey {
    /// The value was not exactly 64 bytes.
    InvalidLength,
    /// The value contained uppercase, non-ASCII, or non-hexadecimal bytes.
    NotLowercaseHex,
}

impl fmt::Display for InvalidIdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => formatter.write_str("idempotency key must be exactly 64 bytes"),
            Self::NotLowercaseHex => {
                formatter.write_str("idempotency key must be lowercase hexadecimal ASCII")
            }
        }
    }
}

impl std::error::Error for InvalidIdempotencyKey {}

/// Derives the stable `UUIDv5` job identifier for a canonical idempotency key.
///
/// The UUID name is the key's 64-byte lowercase wire representation, not its decoded digest.
pub fn deterministic_job_id(key: &IdempotencyKey) -> Uuid {
    Uuid::new_v5(&JOB_ID_NAMESPACE, key.as_str().as_bytes())
}

/// Normalizes keyterms using the JavaScript compatibility contract.
///
/// Values are trimmed, sorted lexicographically by UTF-16 code units (the default JavaScript
/// `Array.prototype.sort` order), and deduplicated. The input count is checked before
/// deduplication so repeated values cannot bypass the public 100-item request bound.
///
/// # Errors
///
/// Returns [`FingerprintError`] when the input count or any normalized value violates the
/// compatibility bounds.
pub fn canonicalize_keyterms(keyterms: Vec<String>) -> Result<Vec<String>, FingerprintError> {
    if keyterms.len() > MAX_KEYTERMS {
        return Err(FingerprintError::TooManyKeyterms);
    }
    let mut canonical = keyterms
        .into_iter()
        .map(|keyterm| keyterm.trim().to_owned())
        .collect::<Vec<_>>();
    for keyterm in &canonical {
        validate_keyterm(keyterm)?;
    }
    canonical.sort_by(|left, right| compare_utf16(left, right));
    canonical.dedup();
    Ok(canonical)
}

/// Computes the frozen `VoiceText` v2/v3 request fingerprint.
///
/// `canonical_keyterms` must already be sorted by Rust string ordering, deduplicated, trimmed,
/// non-empty, and within the public request bounds. Requiring canonical input keeps provider
/// normalization outside this compatibility primitive while preserving the current contract's
/// exact order semantics.
///
/// # Errors
///
/// Returns [`FingerprintError`] for unsupported contracts, non-canonical or oversized input, or
/// empty audio. No hashing is performed over an unbounded request.
pub fn request_fingerprint(
    profile: &BatchProfile,
    canonical_keyterms: &[String],
    audio: &[u8],
) -> Result<BatchRequestFingerprint, FingerprintError> {
    if !matches!(profile.contract_version(), 2 | 3) {
        return Err(FingerprintError::UnsupportedContractVersion);
    }
    for identity in [profile.provider(), profile.model(), profile.language()] {
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(FingerprintError::IdentityTooLong);
        }
    }
    validate_keyterms(canonical_keyterms)?;
    if audio.is_empty() {
        return Err(FingerprintError::EmptyAudio);
    }
    if audio.len() > MAX_AUDIO_BYTES {
        return Err(FingerprintError::AudioTooLarge);
    }

    let contract_version = profile.contract_version().to_string();
    let mut hasher = Sha256::new();
    for identity in [
        contract_version.as_str(),
        profile.provider(),
        profile.model(),
        profile.language(),
    ] {
        write_part(&mut hasher, identity.as_bytes());
    }
    hasher.update((canonical_keyterms.len() as u64).to_be_bytes());
    for keyterm in canonical_keyterms {
        write_part(&mut hasher, keyterm.as_bytes());
    }
    if profile.contract_version() == 2 {
        write_part(&mut hasher, audio);
    } else {
        write_part(&mut hasher, &Sha256::digest(audio));
    }

    Ok(BatchRequestFingerprint::from_bytes(
        hasher.finalize().into(),
    ))
}

fn validate_keyterms(keyterms: &[String]) -> Result<(), FingerprintError> {
    if keyterms.len() > MAX_KEYTERMS {
        return Err(FingerprintError::TooManyKeyterms);
    }
    let mut previous: Option<&str> = None;
    for keyterm in keyterms {
        validate_keyterm(keyterm)?;
        if keyterm.trim() != keyterm {
            return Err(FingerprintError::InvalidKeyterm);
        }
        if previous.is_some_and(|value| compare_utf16(value, keyterm.as_str()) != Ordering::Less) {
            return Err(FingerprintError::NonCanonicalKeyterms);
        }
        previous = Some(keyterm);
    }
    Ok(())
}

fn validate_keyterm(keyterm: &str) -> Result<(), FingerprintError> {
    if keyterm.is_empty() || keyterm.chars().any(char::is_control) {
        return Err(FingerprintError::InvalidKeyterm);
    }
    if keyterm.len() > MAX_KEYTERM_BYTES {
        return Err(FingerprintError::KeytermTooLong);
    }
    Ok(())
}

fn compare_utf16(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn write_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Why a request could not be represented by the frozen fingerprint contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerprintError {
    /// Only frozen contracts 2 and 3 have defined audio representations.
    UnsupportedContractVersion,
    /// A provider, model, or language identity exceeded its compatibility bound.
    IdentityTooLong,
    /// More than 100 canonical keyterms were supplied.
    TooManyKeyterms,
    /// A keyterm was empty, padded, or contained a control character.
    InvalidKeyterm,
    /// A keyterm exceeded 200 UTF-8 bytes.
    KeytermTooLong,
    /// Keyterms were not strictly sorted and deduplicated.
    NonCanonicalKeyterms,
    /// The audio body was empty.
    EmptyAudio,
    /// The audio body exceeded 64 MiB.
    AudioTooLarge,
}

impl fmt::Display for FingerprintError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedContractVersion => "unsupported fingerprint contract version",
            Self::IdentityTooLong => "fingerprint identity is too long",
            Self::TooManyKeyterms => "too many fingerprint keyterms",
            Self::InvalidKeyterm => "invalid fingerprint keyterm",
            Self::KeytermTooLong => "fingerprint keyterm is too long",
            Self::NonCanonicalKeyterms => "fingerprint keyterms are not canonical",
            Self::EmptyAudio => "fingerprint audio is empty",
            Self::AudioTooLarge => "fingerprint audio is too large",
        })
    }
}

impl std::error::Error for FingerprintError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(version: u16, provider: &str, model: &str) -> BatchProfile {
        BatchProfile::new(version, provider, model, "multi").expect("valid test profile")
    }

    #[test]
    fn idempotency_key_is_exact_lowercase_sha256_hex() {
        let value = "0123456789abcdef".repeat(4);
        let key = IdempotencyKey::parse(&value).expect("canonical key");
        assert_eq!(key.as_str(), value);
        assert_eq!(format!("{key:?}"), "IdempotencyKey([REDACTED])");

        assert_eq!(
            IdempotencyKey::parse(&"0".repeat(63)),
            Err(InvalidIdempotencyKey::InvalidLength)
        );
        assert_eq!(
            IdempotencyKey::parse(&"A".repeat(64)),
            Err(InvalidIdempotencyKey::NotLowercaseHex)
        );
        assert_eq!(
            IdempotencyKey::parse(&"g".repeat(64)),
            Err(InvalidIdempotencyKey::NotLowercaseHex)
        );
    }

    #[test]
    fn job_id_has_frozen_uuid_v5_golden_value() {
        let key = IdempotencyKey::parse(&"0".repeat(64)).expect("canonical key");
        assert_eq!(
            deterministic_job_id(&key).hyphenated().to_string(),
            "23302641-8d16-5e1e-953e-0d9c55c1ce65"
        );
    }

    #[test]
    fn v2_fingerprint_matches_private_contract_golden_value() {
        let terms = vec!["Codex".to_owned(), "Deepgram".to_owned()];
        let fingerprint =
            request_fingerprint(&profile(2, "deepgram", "nova-3"), &terms, b"OggSdata")
                .expect("valid request");
        assert_eq!(
            hex::encode(fingerprint.as_bytes()),
            "972fa6c7e086624934866768b8c36b7370762af9b186854cd6c05f7a0559fdf6"
        );
    }

    #[test]
    fn v3_fingerprint_matches_private_contract_golden_value() {
        let terms = vec!["Voice Text".to_owned()];
        let fingerprint =
            request_fingerprint(&profile(3, "elevenlabs", "scribe_v2"), &terms, b"audio")
                .expect("valid request");
        assert_eq!(
            hex::encode(fingerprint.as_bytes()),
            "b7dd8ec50809b00a4e230f1af97498cdce5ed8f0792dcd9b9097ff3261b11248"
        );
    }

    #[test]
    fn keyterm_order_and_duplicates_are_explicitly_rejected() {
        let profile = profile(2, "deepgram", "nova-3");
        let unordered = vec!["Zulu".to_owned(), "Alpha".to_owned()];
        let duplicate = vec!["Alpha".to_owned(), "Alpha".to_owned()];

        assert_eq!(
            request_fingerprint(&profile, &unordered, b"audio"),
            Err(FingerprintError::NonCanonicalKeyterms)
        );
        assert_eq!(
            request_fingerprint(&profile, &duplicate, b"audio"),
            Err(FingerprintError::NonCanonicalKeyterms)
        );
    }

    #[test]
    fn canonical_keyterms_match_javascript_utf16_order() {
        let supplementary = "\u{10000}";
        let private_use_bmp = "\u{e000}";
        let canonical = canonicalize_keyterms(vec![
            format!(" {private_use_bmp} "),
            supplementary.to_owned(),
            supplementary.to_owned(),
        ])
        .expect("valid keyterms");

        assert_eq!(canonical, vec![supplementary, private_use_bmp]);
        assert_eq!(supplementary.cmp(private_use_bmp), Ordering::Greater);
        request_fingerprint(&profile(2, "deepgram", "nova-3"), &canonical, b"audio")
            .expect("JavaScript order is canonical");

        let rust_scalar_order = vec![private_use_bmp.to_owned(), supplementary.to_owned()];
        assert_eq!(
            request_fingerprint(
                &profile(2, "deepgram", "nova-3"),
                &rust_scalar_order,
                b"audio"
            ),
            Err(FingerprintError::NonCanonicalKeyterms)
        );
    }

    #[test]
    fn canonicalization_enforces_bounds_before_deduplication() {
        assert_eq!(
            canonicalize_keyterms(vec!["same".to_owned(); MAX_KEYTERMS + 1]),
            Err(FingerprintError::TooManyKeyterms)
        );
        assert_eq!(
            canonicalize_keyterms(vec!["x".repeat(MAX_KEYTERM_BYTES + 1)]),
            Err(FingerprintError::KeytermTooLong)
        );
        assert_eq!(
            canonicalize_keyterms(vec!["line\nbreak".to_owned()]),
            Err(FingerprintError::InvalidKeyterm)
        );
    }

    #[test]
    fn contract_and_body_bounds_are_enforced() {
        assert_eq!(
            request_fingerprint(&profile(1, "deepgram", "nova-3"), &[], b"audio"),
            Err(FingerprintError::UnsupportedContractVersion)
        );
        assert_eq!(
            request_fingerprint(&profile(2, "deepgram", "nova-3"), &[], b""),
            Err(FingerprintError::EmptyAudio)
        );
    }
}
