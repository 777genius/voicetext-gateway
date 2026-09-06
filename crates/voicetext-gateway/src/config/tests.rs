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
    assert_eq!(config.shutdown_drain_timeout, Duration::from_secs(245));
    assert_eq!(config.max_connections, 128);
    assert_eq!(config.max_upload_bytes, 64 * 1024 * 1024);
    assert_eq!(config.deepgram_api_key_file, None);
    assert_eq!(config.elevenlabs_api_key_file, None);
    assert_eq!(
        config.provider_endpoints.deepgram_live,
        DEFAULT_DEEPGRAM_LIVE_ENDPOINT
    );
    assert!(!config.allow_insecure_provider_endpoints);
    assert_eq!(config.qualification_observation, None);
}

#[test]
fn qualification_observation_is_explicit_and_paired() {
    let mut values = required();
    values.insert(QUALIFICATION_OBSERVATION_DIR_ENV, "/tmp/qualified".into());
    assert_eq!(
        load(&values),
        Err(ConfigError::IncompleteQualificationObservation)
    );
    values.insert(QUALIFICATION_CAMPAIGN_ENV, "synthetic_2026-09-04".into());
    let configured = load(&values).unwrap().qualification_observation.unwrap();
    assert_eq!(configured.directory, PathBuf::from("/tmp/qualified"));
    assert_eq!(configured.campaign, "synthetic_2026-09-04");
    values.insert(QUALIFICATION_CAMPAIGN_ENV, "../escape".into());
    assert!(load(&values).is_err());
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
        (SHUTDOWN_DRAIN_TIMEOUT_ENV, "120000".into()),
        (MAX_CONNECTIONS_ENV, "32".into()),
        (MAX_UPLOAD_BYTES_ENV, (2 * 1024 * 1024).to_string()),
    ]);
    let config = load(&values).unwrap();
    assert_eq!(config.bind_address, "127.0.0.1:9080".parse().unwrap());
    assert_eq!(config.finalize_timeout, Duration::from_millis(750));
    assert_eq!(config.shutdown_drain_timeout, Duration::from_mins(2));
    assert_eq!(config.max_connections, 32);
    assert_eq!(config.max_upload_bytes, 2 * 1024 * 1024);
    assert_eq!(
        config.deepgram_api_key_file,
        Some(SecretSource::File(PathBuf::from("/run/secrets/deepgram")))
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
        (SHUTDOWN_DRAIN_TIMEOUT_ENV, "999"),
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

#[cfg(target_os = "linux")]
#[test]
fn explicit_descriptor_slots_reject_ambiguous_values_and_conflicts() {
    for (slot, name) in SECRET_FD_ENVS.into_iter().enumerate() {
        for bad in [
            "",
            "0",
            "2",
            "03",
            "+3",
            "-3",
            " 3",
            "3 ",
            "3\n",
            "2147483647",
            "999999999999",
        ] {
            let mut values = required();
            values.remove(SECRET_FILE_ENVS[slot]);
            values.insert(name, bad.into());
            assert!(load(&values).is_err(), "slot {slot}");
        }
        let mut values = required();
        values.insert(SECRET_FILE_ENVS[slot], "/run/secrets/key".into());
        values.insert(name, "3".into());
        assert!(load(&values).is_err());
    }
    let mut values = required();
    for (slot, name) in SECRET_FD_ENVS.into_iter().enumerate() {
        values.remove(SECRET_FILE_ENVS[slot]);
        values.insert(name, (slot + 3).to_string());
    }
    assert!(load(&values).is_ok());
    values.insert(SECRET_FD_ENVS[3], "3".into());
    assert!(load(&values).is_err());
}
