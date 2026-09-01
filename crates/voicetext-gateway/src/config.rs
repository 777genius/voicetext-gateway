//! Bounded, non-secret runtime configuration for gateway composition.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::http::Uri;
use thiserror::Error;

/// Environment variable selecting the gateway listen address.
pub const BIND_ADDRESS_ENV: &str = "VOICETEXT_BIND_ADDR";
/// Environment variable containing the absolute `PostgreSQL` URL secret-file path.
pub const POSTGRES_URL_FILE_ENV: &str = "VOICETEXT_POSTGRES_URL_FILE";
/// Environment variable containing the absolute gateway bearer-token file path.
pub const BEARER_TOKEN_FILE_ENV: &str = "VOICETEXT_BEARER_TOKEN_FILE";
/// Environment variable containing the absolute durable audio-spool directory.
pub const SPOOL_DIRECTORY_ENV: &str = "VOICETEXT_SPOOL_DIR";
/// Optional absolute Deepgram API-key file path.
pub const DEEPGRAM_API_KEY_FILE_ENV: &str = "VOICETEXT_DEEPGRAM_API_KEY_FILE";
/// Optional absolute `ElevenLabs` API-key file path.
pub const ELEVENLABS_API_KEY_FILE_ENV: &str = "VOICETEXT_ELEVENLABS_API_KEY_FILE";
/// Optional Deepgram batch endpoint override.
pub const DEEPGRAM_BATCH_ENDPOINT_ENV: &str = "VOICETEXT_DEEPGRAM_BATCH_ENDPOINT";
/// Optional Deepgram live endpoint override.
pub const DEEPGRAM_LIVE_ENDPOINT_ENV: &str = "VOICETEXT_DEEPGRAM_LIVE_ENDPOINT";
/// Optional `ElevenLabs` batch endpoint override.
pub const ELEVENLABS_BATCH_ENDPOINT_ENV: &str = "VOICETEXT_ELEVENLABS_BATCH_ENDPOINT";
/// Optional `ElevenLabs` live endpoint override.
pub const ELEVENLABS_LIVE_ENDPOINT_ENV: &str = "VOICETEXT_ELEVENLABS_LIVE_ENDPOINT";
/// Explicit local-test escape hatch for `http` and `ws` provider endpoints.
pub const ALLOW_INSECURE_ENDPOINTS_ENV: &str = "VOICETEXT_ALLOW_INSECURE_PROVIDER_ENDPOINTS";
/// Final provider-result drain timeout in milliseconds.
pub const FINALIZE_TIMEOUT_ENV: &str = "VOICETEXT_FINALIZE_TIMEOUT_MS";
/// Maximum concurrent inbound connections.
pub const MAX_CONNECTIONS_ENV: &str = "VOICETEXT_MAX_CONNECTIONS";
/// Maximum accepted batch upload size in bytes.
pub const MAX_UPLOAD_BYTES_ENV: &str = "VOICETEXT_MAX_UPLOAD_BYTES";

const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0:8080";
const DEFAULT_DEEPGRAM_BATCH_ENDPOINT: &str = "https://api.deepgram.com/v1/listen";
const DEFAULT_DEEPGRAM_LIVE_ENDPOINT: &str = "wss://api.deepgram.com/v1/listen";
const DEFAULT_ELEVENLABS_BATCH_ENDPOINT: &str = "https://api.elevenlabs.io/v1/speech-to-text";
const DEFAULT_ELEVENLABS_LIVE_ENDPOINT: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";
const DEFAULT_FINALIZE_TIMEOUT_MILLIS: u64 = 5_000;
const MIN_FINALIZE_TIMEOUT_MILLIS: u64 = 250;
const MAX_FINALIZE_TIMEOUT_MILLIS: u64 = 30_000;
const DEFAULT_MAX_CONNECTIONS: usize = 128;
const MAX_MAX_CONNECTIONS: usize = 10_000;
const DEFAULT_MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
const MIN_MAX_UPLOAD_BYTES: usize = 1024 * 1024;
const MAX_MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_ENDPOINT_BYTES: usize = 2_048;

/// Validated runtime configuration. Secret contents are loaded separately.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayConfig {
    /// Gateway HTTP/WebSocket listen address.
    pub bind_address: SocketAddr,
    /// Mounted file containing the `PostgreSQL` connection URL.
    pub postgres_url_file: PathBuf,
    /// Mounted file containing the gateway machine bearer token.
    pub bearer_token_file: PathBuf,
    /// Durable directory holding accepted authoritative batch audio.
    pub spool_directory: PathBuf,
    /// Optional mounted Deepgram API-key file.
    pub deepgram_api_key_file: Option<PathBuf>,
    /// Optional mounted `ElevenLabs` API-key file.
    pub elevenlabs_api_key_file: Option<PathBuf>,
    /// Provider endpoints selected by composition.
    pub provider_endpoints: ProviderEndpoints,
    /// Maximum time to drain final provider results after finalize begins.
    pub finalize_timeout: Duration,
    /// Maximum concurrent inbound connections.
    pub max_connections: usize,
    /// Maximum accepted batch upload size.
    pub max_upload_bytes: usize,
    /// Whether local-test plaintext provider transports are permitted.
    pub allow_insecure_provider_endpoints: bool,
}

/// Validated provider transport endpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderEndpoints {
    /// Deepgram batch HTTPS endpoint.
    pub deepgram_batch: String,
    /// Deepgram live WebSocket endpoint.
    pub deepgram_live: String,
    /// `ElevenLabs` batch HTTPS endpoint.
    pub elevenlabs_batch: String,
    /// `ElevenLabs` live WebSocket endpoint.
    pub elevenlabs_live: String,
}

impl GatewayConfig {
    /// Loads configuration from the process environment without reading secret files.
    ///
    /// # Errors
    ///
    /// Returns a typed, value-redacting error when required variables are absent or
    /// when a value falls outside the documented syntax and resource bounds.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    /// Loads configuration through an injected lookup function.
    ///
    /// This keeps tests deterministic and avoids process-global environment mutation.
    /// Missing optional provider key paths disable that provider at composition time.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] without retaining or echoing rejected values.
    pub fn from_lookup(
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let allow_insecure_provider_endpoints = parse_boolean(
            optional(&mut lookup, ALLOW_INSECURE_ENDPOINTS_ENV)?,
            ALLOW_INSECURE_ENDPOINTS_ENV,
            false,
        )?;
        let provider_endpoints = ProviderEndpoints {
            deepgram_batch: parse_endpoint(
                optional(&mut lookup, DEEPGRAM_BATCH_ENDPOINT_ENV)?
                    .unwrap_or_else(|| DEFAULT_DEEPGRAM_BATCH_ENDPOINT.to_owned()),
                DEEPGRAM_BATCH_ENDPOINT_ENV,
                EndpointKind::Http,
                allow_insecure_provider_endpoints,
            )?,
            deepgram_live: parse_endpoint(
                optional(&mut lookup, DEEPGRAM_LIVE_ENDPOINT_ENV)?
                    .unwrap_or_else(|| DEFAULT_DEEPGRAM_LIVE_ENDPOINT.to_owned()),
                DEEPGRAM_LIVE_ENDPOINT_ENV,
                EndpointKind::WebSocket,
                allow_insecure_provider_endpoints,
            )?,
            elevenlabs_batch: parse_endpoint(
                optional(&mut lookup, ELEVENLABS_BATCH_ENDPOINT_ENV)?
                    .unwrap_or_else(|| DEFAULT_ELEVENLABS_BATCH_ENDPOINT.to_owned()),
                ELEVENLABS_BATCH_ENDPOINT_ENV,
                EndpointKind::Http,
                allow_insecure_provider_endpoints,
            )?,
            elevenlabs_live: parse_endpoint(
                optional(&mut lookup, ELEVENLABS_LIVE_ENDPOINT_ENV)?
                    .unwrap_or_else(|| DEFAULT_ELEVENLABS_LIVE_ENDPOINT.to_owned()),
                ELEVENLABS_LIVE_ENDPOINT_ENV,
                EndpointKind::WebSocket,
                allow_insecure_provider_endpoints,
            )?,
        };

        Ok(Self {
            bind_address: optional(&mut lookup, BIND_ADDRESS_ENV)?
                .unwrap_or_else(|| DEFAULT_BIND_ADDRESS.to_owned())
                .parse()
                .map_err(|_| ConfigError::InvalidSocketAddress {
                    name: BIND_ADDRESS_ENV,
                })?,
            postgres_url_file: required_path(&mut lookup, POSTGRES_URL_FILE_ENV)?,
            bearer_token_file: required_path(&mut lookup, BEARER_TOKEN_FILE_ENV)?,
            spool_directory: required_path(&mut lookup, SPOOL_DIRECTORY_ENV)?,
            deepgram_api_key_file: optional_path(&mut lookup, DEEPGRAM_API_KEY_FILE_ENV)?,
            elevenlabs_api_key_file: optional_path(&mut lookup, ELEVENLABS_API_KEY_FILE_ENV)?,
            provider_endpoints,
            finalize_timeout: Duration::from_millis(parse_bounded_u64(
                optional(&mut lookup, FINALIZE_TIMEOUT_ENV)?,
                FINALIZE_TIMEOUT_ENV,
                DEFAULT_FINALIZE_TIMEOUT_MILLIS,
                MIN_FINALIZE_TIMEOUT_MILLIS,
                MAX_FINALIZE_TIMEOUT_MILLIS,
            )?),
            max_connections: parse_bounded_usize(
                optional(&mut lookup, MAX_CONNECTIONS_ENV)?,
                MAX_CONNECTIONS_ENV,
                DEFAULT_MAX_CONNECTIONS,
                1,
                MAX_MAX_CONNECTIONS,
            )?,
            max_upload_bytes: parse_bounded_usize(
                optional(&mut lookup, MAX_UPLOAD_BYTES_ENV)?,
                MAX_UPLOAD_BYTES_ENV,
                DEFAULT_MAX_UPLOAD_BYTES,
                MIN_MAX_UPLOAD_BYTES,
                MAX_MAX_UPLOAD_BYTES,
            )?,
            allow_insecure_provider_endpoints,
        })
    }
}

/// Safe configuration failure which never includes a rejected value.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    /// A required variable is absent.
    #[error("required environment variable {name} is missing")]
    Missing { name: &'static str },
    /// A variable is empty, oversized, or contains control bytes.
    #[error("environment variable {name} contains an invalid bounded value")]
    InvalidValue { name: &'static str },
    /// A configured filesystem location is not absolute.
    #[error("path in environment variable {name} must be absolute")]
    RelativePath { name: &'static str },
    /// The listen address is not a numeric socket address.
    #[error("environment variable {name} must contain a valid socket address")]
    InvalidSocketAddress { name: &'static str },
    /// A boolean is not exactly `true` or `false`.
    #[error("environment variable {name} must be true or false")]
    InvalidBoolean { name: &'static str },
    /// An integer is malformed or outside its safe range.
    #[error("environment variable {name} must be an integer from {minimum} through {maximum}")]
    OutOfRange {
        name: &'static str,
        minimum: u64,
        maximum: u64,
    },
    /// A provider endpoint has the wrong structure or transport scheme.
    #[error("environment variable {name} must contain a valid secure provider endpoint")]
    InvalidEndpoint { name: &'static str },
}

#[derive(Clone, Copy)]
enum EndpointKind {
    Http,
    WebSocket,
}

fn required_path(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    name: &'static str,
) -> Result<PathBuf, ConfigError> {
    let value = optional(lookup, name)?.ok_or(ConfigError::Missing { name })?;
    validate_path(value, name)?.ok_or(ConfigError::Missing { name })
}

fn optional_path(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    name: &'static str,
) -> Result<Option<PathBuf>, ConfigError> {
    let value = optional(lookup, name)?;
    validate_path_option(value, name)
}

fn validate_path(value: String, name: &'static str) -> Result<Option<PathBuf>, ConfigError> {
    validate_path_option(Some(value), name)
}

fn validate_path_option(
    value: Option<String>,
    name: &'static str,
) -> Result<Option<PathBuf>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() || value.len() > MAX_PATH_BYTES || value.chars().any(char::is_control) {
        return Err(ConfigError::InvalidValue { name });
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(ConfigError::RelativePath { name });
    }
    Ok(Some(path))
}

fn optional(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    name: &'static str,
) -> Result<Option<String>, ConfigError> {
    let value = lookup(name);
    if value
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.chars().any(char::is_control))
    {
        return Err(ConfigError::InvalidValue { name });
    }
    Ok(value)
}

fn parse_boolean(
    value: Option<String>,
    name: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    match value {
        None => Ok(default),
        Some(value) if value == "true" => Ok(true),
        Some(value) if value == "false" => Ok(false),
        Some(_) => Err(ConfigError::InvalidBoolean { name }),
    }
}

fn parse_bounded_u64(
    value: Option<String>,
    name: &'static str,
    default: u64,
    minimum: u64,
    maximum: u64,
) -> Result<u64, ConfigError> {
    let parsed = value
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| ConfigError::OutOfRange {
            name,
            minimum,
            maximum,
        })?
        .unwrap_or(default);
    if !(minimum..=maximum).contains(&parsed) {
        return Err(ConfigError::OutOfRange {
            name,
            minimum,
            maximum,
        });
    }
    Ok(parsed)
}

fn parse_bounded_usize(
    value: Option<String>,
    name: &'static str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, ConfigError> {
    let parsed = value
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| ConfigError::OutOfRange {
            name,
            minimum: minimum as u64,
            maximum: maximum as u64,
        })?
        .unwrap_or(default);
    if !(minimum..=maximum).contains(&parsed) {
        return Err(ConfigError::OutOfRange {
            name,
            minimum: minimum as u64,
            maximum: maximum as u64,
        });
    }
    Ok(parsed)
}

fn parse_endpoint(
    value: String,
    name: &'static str,
    kind: EndpointKind,
    allow_insecure: bool,
) -> Result<String, ConfigError> {
    if value.len() > MAX_ENDPOINT_BYTES || value.chars().any(char::is_control) {
        return Err(ConfigError::InvalidEndpoint { name });
    }
    let uri = value
        .parse::<Uri>()
        .map_err(|_| ConfigError::InvalidEndpoint { name })?;
    let scheme = uri
        .scheme_str()
        .ok_or(ConfigError::InvalidEndpoint { name })?;
    let authority = uri
        .authority()
        .ok_or(ConfigError::InvalidEndpoint { name })?;
    if authority.host().is_empty() || authority.as_str().contains('@') || uri.path().is_empty() {
        return Err(ConfigError::InvalidEndpoint { name });
    }
    let secure = match kind {
        EndpointKind::Http => scheme == "https",
        EndpointKind::WebSocket => scheme == "wss",
    };
    let permitted_insecure = allow_insecure
        && match kind {
            EndpointKind::Http => scheme == "http",
            EndpointKind::WebSocket => scheme == "ws",
        };
    if !secure && !permitted_insecure {
        return Err(ConfigError::InvalidEndpoint { name });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn required() -> HashMap<&'static str, String> {
        HashMap::from([
            (POSTGRES_URL_FILE_ENV, "/run/secrets/postgres-url".into()),
            (BEARER_TOKEN_FILE_ENV, "/run/secrets/gateway-token".into()),
            (SPOOL_DIRECTORY_ENV, "/var/lib/voicetext/spool".into()),
        ])
    }

    fn load(values: &HashMap<&'static str, String>) -> Result<GatewayConfig, ConfigError> {
        GatewayConfig::from_lookup(|name| values.get(name).cloned())
    }

    #[test]
    fn defaults_are_secure_and_providers_are_optional() {
        let config = load(&required()).unwrap();
        assert_eq!(config.bind_address, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.finalize_timeout, Duration::from_secs(5));
        assert_eq!(config.max_connections, 128);
        assert_eq!(config.max_upload_bytes, 64 * 1024 * 1024);
        assert_eq!(config.deepgram_api_key_file, None);
        assert_eq!(config.elevenlabs_api_key_file, None);
        assert_eq!(
            config.provider_endpoints.deepgram_live,
            DEFAULT_DEEPGRAM_LIVE_ENDPOINT
        );
        assert!(!config.allow_insecure_provider_endpoints);
    }

    #[test]
    fn accepts_bounded_overrides_and_absolute_provider_secret_paths() {
        let mut values = required();
        values.extend([
            (BIND_ADDRESS_ENV, "127.0.0.1:9080".into()),
            (DEEPGRAM_API_KEY_FILE_ENV, "/run/secrets/deepgram".into()),
            (
                ELEVENLABS_API_KEY_FILE_ENV,
                "/run/secrets/elevenlabs".into(),
            ),
            (FINALIZE_TIMEOUT_ENV, "750".into()),
            (MAX_CONNECTIONS_ENV, "32".into()),
            (MAX_UPLOAD_BYTES_ENV, (2 * 1024 * 1024).to_string()),
        ]);
        let config = load(&values).unwrap();
        assert_eq!(config.bind_address, "127.0.0.1:9080".parse().unwrap());
        assert_eq!(config.finalize_timeout, Duration::from_millis(750));
        assert_eq!(config.max_connections, 32);
        assert_eq!(config.max_upload_bytes, 2 * 1024 * 1024);
        assert_eq!(
            config.deepgram_api_key_file,
            Some(PathBuf::from("/run/secrets/deepgram"))
        );
    }

    #[test]
    fn requires_absolute_non_control_paths_without_reading_them() {
        let mut missing = required();
        missing.remove(POSTGRES_URL_FILE_ENV);
        assert_eq!(
            load(&missing),
            Err(ConfigError::Missing {
                name: POSTGRES_URL_FILE_ENV
            })
        );

        for rejected in ["relative/secret", "/run/secrets/key\n"] {
            let mut values = required();
            values.insert(BEARER_TOKEN_FILE_ENV, rejected.into());
            assert!(load(&values).is_err());
        }
    }

    #[test]
    fn plaintext_provider_endpoints_need_explicit_test_escape_hatch() {
        let mut values = required();
        values.insert(
            DEEPGRAM_BATCH_ENDPOINT_ENV,
            "http://127.0.0.1:8090/v1/listen".into(),
        );
        assert_eq!(
            load(&values),
            Err(ConfigError::InvalidEndpoint {
                name: DEEPGRAM_BATCH_ENDPOINT_ENV
            })
        );

        values.insert(ALLOW_INSECURE_ENDPOINTS_ENV, "true".into());
        values.insert(
            DEEPGRAM_LIVE_ENDPOINT_ENV,
            "ws://127.0.0.1:8091/v1/listen".into(),
        );
        let config = load(&values).unwrap();
        assert!(config.allow_insecure_provider_endpoints);
    }

    #[test]
    fn rejects_wrong_schemes_credentials_and_invalid_bounds() {
        for (name, value) in [
            (DEEPGRAM_BATCH_ENDPOINT_ENV, "wss://api.example.test/path"),
            (DEEPGRAM_LIVE_ENDPOINT_ENV, "https://api.example.test/path"),
            (
                ELEVENLABS_BATCH_ENDPOINT_ENV,
                "https://user:password@api.example.test/path",
            ),
        ] {
            let mut values = required();
            values.insert(name, value.into());
            assert!(load(&values).is_err());
        }

        for (name, value) in [
            (FINALIZE_TIMEOUT_ENV, "249"),
            (MAX_CONNECTIONS_ENV, "0"),
            (MAX_UPLOAD_BYTES_ENV, "67108865"),
        ] {
            let mut values = required();
            values.insert(name, value.into());
            assert!(matches!(load(&values), Err(ConfigError::OutOfRange { .. })));
        }
    }

    #[test]
    fn errors_never_echo_rejected_values() {
        let mut values = required();
        let rejected = "relative-super-secret-location";
        values.insert(POSTGRES_URL_FILE_ENV, rejected.into());
        let rendered = load(&values).unwrap_err().to_string();
        assert!(!rendered.contains(rejected));

        values.insert(POSTGRES_URL_FILE_ENV, "/run/secrets/postgres".into());
        let endpoint_secret = "https://user:password@api.example.test/path";
        values.insert(DEEPGRAM_BATCH_ENDPOINT_ENV, endpoint_secret.into());
        let rendered = load(&values).unwrap_err().to_string();
        assert!(!rendered.contains(endpoint_secret));
    }
}
