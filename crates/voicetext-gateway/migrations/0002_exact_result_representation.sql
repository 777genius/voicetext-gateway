-- Expand without changing the legacy jsonb column used by the previous binary. The exact compact
-- serialization is stored alongside it; keeping both representations makes binary rollback safe.
ALTER TABLE voicetext_batch_jobs
    DROP CONSTRAINT voicetext_batch_jobs_result_json_check,
    ADD CONSTRAINT voicetext_batch_jobs_result_json_check
        CHECK (result_json IS NULL OR octet_length(result_json::text) <= 33554432),
    ADD COLUMN result_text text,
    ADD CONSTRAINT voicetext_batch_jobs_result_text_check
        CHECK (result_text IS NULL OR octet_length(result_text) <= 33554432);

UPDATE voicetext_batch_jobs
SET result_text = result_json::text
WHERE result_json IS NOT NULL;
