-- Preserve the checksum of the deployed initial migration. Existing jsonb values were already
-- bounded to 8 MiB as rendered text and remain valid under the new 32 MiB exact-text bound.
ALTER TABLE voicetext_batch_jobs
    DROP CONSTRAINT voicetext_batch_jobs_result_json_check,
    ALTER COLUMN result_json TYPE text USING result_json::text,
    ADD CONSTRAINT voicetext_batch_jobs_result_json_check
        CHECK (result_json IS NULL OR octet_length(result_json) <= 33554432);
