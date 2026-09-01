//! Strict machine-to-machine bearer authentication.

use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
use thiserror::Error;

use crate::secret::MachineSecret;

/// Authenticates the one allowed `Authorization` header.
///
/// # Errors
///
/// Returns a deliberately non-specific error for invalid credentials and typed
/// structural errors for missing, repeated, or malformed headers.
pub fn authenticate(
    headers: &HeaderMap,
    expected: &MachineSecret,
) -> Result<(), AuthenticationError> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next().ok_or(AuthenticationError::Missing)?;
    if values.next().is_some() {
        return Err(AuthenticationError::Multiple);
    }
    let candidate = parse_bearer(value)?;
    if !expected.verifies(candidate) {
        return Err(AuthenticationError::InvalidCredentials);
    }
    Ok(())
}

/// Parses an exact `Bearer <visible-ASCII-token>` header without allocating.
///
/// # Errors
///
/// Returns [`AuthenticationError::Malformed`] unless the header contains the
/// exact case-sensitive scheme, one ASCII space, and one non-empty token.
pub fn parse_bearer(value: &HeaderValue) -> Result<&[u8], AuthenticationError> {
    const PREFIX: &[u8] = b"Bearer ";
    let bytes = value.as_bytes();
    let token = bytes
        .strip_prefix(PREFIX)
        .ok_or(AuthenticationError::Malformed)?;
    if token.is_empty() || token.iter().any(|byte| !(0x21..=0x7e).contains(byte)) {
        return Err(AuthenticationError::Malformed);
    }
    Ok(token)
}

/// Safe machine-authentication failure.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum AuthenticationError {
    /// The caller supplied no authorization header.
    #[error("authorization header is required")]
    Missing,
    /// Ambiguous duplicate authorization headers are not accepted.
    #[error("multiple authorization headers are not allowed")]
    Multiple,
    /// The header does not match the required syntax.
    #[error("authorization header is malformed")]
    Malformed,
    /// The presented token does not authenticate.
    #[error("credentials are invalid")]
    InvalidCredentials,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_exact_bearer_form() {
        let expected = MachineSecret::from_token(b"gateway-token").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer gateway-token"),
        );
        assert_eq!(authenticate(&headers, &expected), Ok(()));
    }

    #[test]
    fn rejects_missing_duplicate_and_invalid_credentials() {
        let expected = MachineSecret::from_token(b"gateway-token").unwrap();
        assert_eq!(
            authenticate(&HeaderMap::new(), &expected),
            Err(AuthenticationError::Missing)
        );

        let mut duplicate = HeaderMap::new();
        duplicate.append(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer gateway-token"),
        );
        duplicate.append(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer gateway-token"),
        );
        assert_eq!(
            authenticate(&duplicate, &expected),
            Err(AuthenticationError::Multiple)
        );

        let mut invalid = HeaderMap::new();
        invalid.insert(AUTHORIZATION, HeaderValue::from_static("Bearer short"));
        assert_eq!(
            authenticate(&invalid, &expected),
            Err(AuthenticationError::InvalidCredentials)
        );
    }

    #[test]
    fn rejects_malformed_headers() {
        for malformed in [
            "bearer token",
            "BEARER token",
            "Bearer",
            "Bearer ",
            "Bearer  token",
            " Bearer token",
            "Bearer token suffix",
            "Basic token",
        ] {
            let value = HeaderValue::from_str(malformed).unwrap();
            assert_eq!(
                parse_bearer(&value),
                Err(AuthenticationError::Malformed),
                "unexpectedly accepted {malformed:?}"
            );
        }
    }
}
