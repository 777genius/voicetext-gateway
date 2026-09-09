CREATE TABLE voicetext_batch_jobs (
    job_id uuid PRIMARY KEY,
    contract_version smallint NOT NULL CHECK (contract_version > 0),
    provider text NOT NULL CHECK (provider <> '' AND octet_length(provider) <= 256),
    model text NOT NULL CHECK (model <> '' AND octet_length(model) <= 256),
    language text NOT NULL CHECK (language <> '' AND octet_length(language) <= 256),
    fingerprint bytea NOT NULL CHECK (octet_length(fingerprint) = 32),
    audio_handle text NOT NULL CHECK (octet_length(audio_handle) = 105),
    authoritative_duration_millis bigint NOT NULL CHECK (authoritative_duration_millis >= 0),
    keyterms jsonb NOT NULL CHECK (jsonb_typeof(keyterms) = 'array' AND octet_length(keyterms::text) <= 65536),
    state text NOT NULL CHECK (state IN ('accepted', 'submitting', 'retryable', 'completed', 'failed', 'outcome_unknown')),
    attempt bigint,
    failure_code text,
    unknown_reason text,
    provider_reference text CHECK (provider_reference IS NULL OR (provider_reference <> '' AND octet_length(provider_reference) <= 256)),
    retry_after_millis bigint CHECK (retry_after_millis BETWEEN 0 AND 3600000),
    result_json jsonb CHECK (result_json IS NULL OR octet_length(result_json::text) <= 8388608),
    revision bigint NOT NULL DEFAULT 0 CHECK (revision >= 0),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK (attempt IS NULL OR attempt BETWEEN 1 AND 4294967295),
    CHECK (failure_code IS NULL OR (failure_code <> '' AND octet_length(failure_code) <= 128)),
    CHECK (unknown_reason IS NULL OR unknown_reason IN ('submission', 'interrupted_submission')),
    CHECK (
        (state = 'accepted' AND attempt IS NULL AND failure_code IS NULL AND unknown_reason IS NULL AND retry_after_millis IS NULL AND result_json IS NULL)
        OR (state = 'submitting' AND attempt IS NOT NULL AND failure_code IS NULL AND unknown_reason IS NULL AND retry_after_millis IS NULL AND result_json IS NULL)
        OR (state = 'retryable' AND attempt IS NOT NULL AND failure_code IS NOT NULL AND unknown_reason IS NULL AND result_json IS NULL)
        OR (state = 'completed' AND attempt IS NOT NULL AND failure_code IS NULL AND unknown_reason IS NULL AND retry_after_millis IS NULL AND result_json IS NOT NULL)
        OR (state = 'failed' AND attempt IS NOT NULL AND failure_code IS NOT NULL AND unknown_reason IS NULL AND retry_after_millis IS NULL AND result_json IS NULL)
        OR (state = 'outcome_unknown' AND attempt IS NOT NULL AND retry_after_millis IS NULL AND result_json IS NULL AND (
            (unknown_reason = 'submission' AND failure_code IS NOT NULL)
            OR (unknown_reason = 'interrupted_submission' AND failure_code IS NULL)
        ))
    )
);

CREATE INDEX voicetext_batch_jobs_recovery_idx
    ON voicetext_batch_jobs (job_id)
    WHERE state IN ('accepted', 'retryable', 'submitting');
