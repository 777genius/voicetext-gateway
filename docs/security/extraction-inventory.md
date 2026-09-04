# Private-backend extraction inventory

Source repository: private `777genius/VoicetextAI_backend` at reviewed commit `f71e53d`.

This document records provenance only. The private Git history will not be imported into this
repository.

## Eligible for selective extraction

The following files contain STT mechanics that can be independently reviewed, divided into small
modules, and covered by new public tests:

- `src/features/transcription/batch.rs`
- `src/features/transcription/streaming.rs`
- `src/features/transcription/deepgram.rs`
- `src/features/transcription/elevenlabs.rs`
- `src/features/transcription/elevenlabs_batch.rs`
- `src/features/transcription/opus.rs`
- `src/features/transcription/opus_decoder.rs`
- `src/features/transcription/ogg_opus.rs`

Provider wire DTOs remain private to their outbound adapter. Existing comments, logs, error text,
configuration reads, and tests are reviewed before reuse rather than copied mechanically.

## Behavioral specification only

These files combine useful invariants with private SaaS responsibilities and must not be copied:

- `src/features/transcription/batch_v2.rs`
- `src/features/transcription/ws_handler.rs`
- `src/api/state.rs`
- `src/bootstrap/config.rs`
- `src/shared/infrastructure/repository/transcription_job.rs`
- `src/shared/infrastructure/repository/transcription_session.rs`

Only independently restated contract, state-transition, retry, finalize, and idempotency behavior may
be implemented in the public gateway.

## Always excluded

- authentication for public users, OAuth, JWT, password and session management;
- licensing, machine licensing, usage quotas, referrals, gifts, payments, and webhooks;
- Redis ownership and SaaS multi-tenant admission;
- customer data, production configuration, endpoints, logs, recordings, and database migrations;
- private deployment manifests and Git history;
- legacy OpenAI/Whisper transcription paths not required by Discord Meeting Assistant.

## Publication gate

The owner must confirm the OSS license and right to publish the selected implementation. A clean-tree
and clean-history secret scan, dependency license audit, vulnerability audit, SBOM, and manual diff
against this inventory are mandatory before the first public push.
