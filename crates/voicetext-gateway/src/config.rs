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
/// Maximum graceful batch-task drain interval in milliseconds.
pub const SHUTDOWN_DRAIN_TIMEOUT_ENV: &str = "VOICETEXT_SHUTDOWN_DRAIN_TIMEOUT_MS";
/// Maximum `PostgreSQL` connections per gateway process (1 through 10).
pub const DATABASE_MAX_CONNECTIONS_ENV: &str = "VOICETEXT_DATABASE_MAX_CONNECTIONS";
/// Maximum concurrent inbound connections.
pub const MAX_CONNECTIONS_ENV: &str = "VOICETEXT_MAX_CONNECTIONS";
/// Maximum accepted batch upload size in bytes.
pub const MAX_UPLOAD_BYTES_ENV: &str = "VOICETEXT_MAX_UPLOAD_BYTES";
/// Opt-in absolute directory for synthetic qualification observation records.
pub const QUALIFICATION_OBSERVATION_DIR_ENV: &str = "VOICETEXT_QUALIFICATION_OBSERVATION_DIR";
/// Bounded pathname-safe synthetic qualification campaign label.
pub const QUALIFICATION_CAMPAIGN_ENV: &str = "VOICETEXT_QUALIFICATION_CAMPAIGN";

const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0:8080";
const DEFAULT_DEEPGRAM_BATCH_ENDPOINT: &str = "https://api.deepgram.com/v1/listen";
const DEFAULT_DEEPGRAM_LIVE_ENDPOINT: &str = "wss://api.deepgram.com/v1/listen";
const DEFAULT_ELEVENLABS_BATCH_ENDPOINT: &str = "https://api.elevenlabs.io/v1/speech-to-text";
const DEFAULT_ELEVENLABS_LIVE_ENDPOINT: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";
const DEFAULT_FINALIZE_TIMEOUT_MILLIS: u64 = 5_000;
const MIN_FINALIZE_TIMEOUT_MILLIS: u64 = 250;
const MAX_FINALIZE_TIMEOUT_MILLIS: u64 = 30_000;
const DEFAULT_SHUTDOWN_DRAIN_TIMEOUT_MILLIS: u64 = 245_000;
const MIN_SHUTDOWN_DRAIN_TIMEOUT_MILLIS: u64 = 1_000;
const MAX_SHUTDOWN_DRAIN_TIMEOUT_MILLIS: u64 = 600_000;
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
    /// File or inherited descriptor containing the `PostgreSQL` connection URL.
    pub postgres_url_file: SecretSource,
    /// File or inherited descriptor containing the gateway machine bearer token.
    pub bearer_token_file: SecretSource,
    /// Durable directory holding accepted authoritative batch audio.
    pub spool_directory: PathBuf,
    /// Optional Deepgram API-key source.
    pub deepgram_api_key_file: Option<SecretSource>,
    /// Optional `ElevenLabs` API-key source.
    pub elevenlabs_api_key_file: Option<SecretSource>,
    /// Provider endpoints selected by composition.
    pub provider_endpoints: ProviderEndpoints,
    /// Maximum time to drain final provider results after finalize begins.
    pub finalize_timeout: Duration,
    /// Maximum time to preserve in-flight paid batch work after shutdown begins.
    pub shutdown_drain_timeout: Duration,
    /// Maximum `PostgreSQL` connections per gateway process.
    pub database_max_connections: u32,
    /// Maximum concurrent inbound connections.
    pub max_connections: usize,
    /// Maximum accepted batch upload size.
    pub max_upload_bytes: usize,
    /// Whether local-test plaintext provider transports are permitted.
    pub allow_insecure_provider_endpoints: bool,
    /// Absent by default; both values must be explicitly configured to enable observations.
    pub qualification_observation: Option<QualificationObservationConfig>,
}

/// Validated opt-in local qualification observation destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QualificationObservationConfig {
    pub directory: PathBuf,
    pub campaign: String,
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
        Self::from_lookup(|name| match env::var(name) {
            Ok(value) => Some(value),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => Some("\0".into()),
        })
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
        let qualification_observation = qualification_observation(&mut lookup)?;
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

        let config = Self {
            bind_address: optional(&mut lookup, BIND_ADDRESS_ENV)?
                .unwrap_or_else(|| DEFAULT_BIND_ADDRESS.to_owned())
                .parse()
                .map_err(|_| ConfigError::InvalidSocketAddress {
                    name: BIND_ADDRESS_ENV,
                })?,
            postgres_url_file: secret_source(&mut lookup, 1)?.ok_or(ConfigError::Missing {
                name: POSTGRES_URL_FILE_ENV,
            })?,
            bearer_token_file: secret_source(&mut lookup, 0)?.ok_or(ConfigError::Missing {
                name: BEARER_TOKEN_FILE_ENV,
            })?,
            spool_directory: required_path(&mut lookup, SPOOL_DIRECTORY_ENV)?,
            deepgram_api_key_file: secret_source(&mut lookup, 2)?,
            elevenlabs_api_key_file: secret_source(&mut lookup, 3)?,
            provider_endpoints,
            finalize_timeout: Duration::from_millis(parse_bounded_u64(
                optional(&mut lookup, FINALIZE_TIMEOUT_ENV)?,
                FINALIZE_TIMEOUT_ENV,
                DEFAULT_FINALIZE_TIMEOUT_MILLIS,
                MIN_FINALIZE_TIMEOUT_MILLIS,
                MAX_FINALIZE_TIMEOUT_MILLIS,
            )?),
            shutdown_drain_timeout: Duration::from_millis(parse_bounded_u64(
                optional(&mut lookup, SHUTDOWN_DRAIN_TIMEOUT_ENV)?,
                SHUTDOWN_DRAIN_TIMEOUT_ENV,
                DEFAULT_SHUTDOWN_DRAIN_TIMEOUT_MILLIS,
                MIN_SHUTDOWN_DRAIN_TIMEOUT_MILLIS,
                MAX_SHUTDOWN_DRAIN_TIMEOUT_MILLIS,
            )?),
            database_max_connections: u32::try_from(parse_bounded_u64(
                optional(&mut lookup, DATABASE_MAX_CONNECTIONS_ENV)?,
                DATABASE_MAX_CONNECTIONS_ENV,
                10,
                1,
                10,
            )?)
            .map_err(|_| ConfigError::OutOfRange {
                name: DATABASE_MAX_CONNECTIONS_ENV,
                minimum: 1,
                maximum: 10,
            })?,
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
            qualification_observation,
        };
        config.validate_sources()?;
        Ok(config)
    }

    fn validate_sources(&self) -> Result<(), ConfigError> {
        let sources = [
            Some(&self.bearer_token_file),
            Some(&self.postgres_url_file),
            self.deepgram_api_key_file.as_ref(),
            self.elevenlabs_api_key_file.as_ref(),
        ];
        let mut descriptors = Vec::new();
        for (slot, source) in sources.into_iter().enumerate() {
            if let Some(SecretSource::Descriptor(fd)) = source {
                if descriptors.contains(fd) {
                    return Err(ConfigError::InvalidValue {
                        name: SECRET_FD_ENVS[slot],
                    });
                }
                descriptors.push(*fd);
            }
        }
        Ok(())
    }
}

/// Safe configuration failure which never includes a rejected value.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    /// Inherited descriptor capture is implemented only for Linux.
    #[error("inherited secret descriptors are unsupported on this platform")]
    UnsupportedDescriptorPlatform,
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
    /// Qualification directory and campaign must be supplied together.
    #[error("qualification observation directory and campaign must be configured together")]
    IncompleteQualificationObservation,
}

fn qualification_observation(
    lookup: &mut impl FnMut(&str) -> Option<String>,
) -> Result<Option<QualificationObservationConfig>, ConfigError> {
    let directory = optional_path(lookup, QUALIFICATION_OBSERVATION_DIR_ENV)?;
    let campaign = optional(lookup, QUALIFICATION_CAMPAIGN_ENV)?;
    match (directory, campaign) {
        (None, None) => Ok(None),
        (Some(directory), Some(campaign))
            if (1..=64).contains(&campaign.len())
                && campaign
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) =>
        {
            Ok(Some(QualificationObservationConfig {
                directory,
                campaign,
            }))
        }
        (Some(_), Some(_)) => Err(ConfigError::InvalidValue {
            name: QUALIFICATION_CAMPAIGN_ENV,
        }),
        _ => Err(ConfigError::IncompleteQualificationObservation),
    }
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
mod tests;

/// Fixed launcher capability slots: bearer, database, Deepgram, `ElevenLabs`.
pub const SECRET_FD_ENVS: [&str; 4] = [
    "VOICETEXT_BEARER_TOKEN_FD",
    "VOICETEXT_POSTGRES_URL_FD",
    "VOICETEXT_DEEPGRAM_API_KEY_FD",
    "VOICETEXT_ELEVENLABS_API_KEY_FD",
];
const SECRET_FILE_ENVS: [&str; 4] = [
    BEARER_TOKEN_FILE_ENV,
    POSTGRES_URL_FILE_ENV,
    DEEPGRAM_API_KEY_FILE_ENV,
    ELEVENLABS_API_KEY_FILE_ENV,
];

/// Explicit named-file or inherited capability source; never a proc-fd pathname.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretSource {
    File(PathBuf),
    Descriptor(i32),
}

fn parse_descriptor(value: &str, name: &'static str) -> Result<i32, ConfigError> {
    let invalid = || ConfigError::InvalidValue { name };
    if !cfg!(target_os = "linux") {
        return Err(ConfigError::UnsupportedDescriptorPlatform);
    }
    if value.len() > 10 || value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let fd: i32 = value.parse().map_err(|_| invalid())?;
    if !(3..i32::MAX).contains(&fd) {
        return Err(invalid());
    }
    Ok(fd)
}

fn secret_source(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    slot: usize,
) -> Result<Option<SecretSource>, ConfigError> {
    let file = optional_path(lookup, SECRET_FILE_ENVS[slot])?;
    let fd = optional(lookup, SECRET_FD_ENVS[slot])?;
    match (file, fd) {
        (Some(_), Some(_)) => Err(ConfigError::InvalidValue {
            name: SECRET_FD_ENVS[slot],
        }),
        (Some(path), None) => Ok(Some(SecretSource::File(path))),
        (None, Some(value)) => Ok(Some(SecretSource::Descriptor(parse_descriptor(
            &value,
            SECRET_FD_ENVS[slot],
        )?))),
        (None, None) => Ok(None),
    }
}
