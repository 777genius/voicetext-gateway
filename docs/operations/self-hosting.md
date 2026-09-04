# Self-hosting, security, and troubleshooting

This runbook complements the root quick start. It describes what crosses each boundary, which data
is retained, and how to diagnose a deployment without exposing credentials.

## Data flow and trust boundaries

```text
Discord Meeting Assistant
  | machine bearer token + VoiceText contract
  v
TLS edge (optional Caddy) -> VoiceText Gateway
                              | batch HTTPS / live WSS with provider key
                              v
                        Deepgram or ElevenLabs

VoiceText Gateway -> PostgreSQL: batch identity, state, result, bounded diagnostics
VoiceText Gateway -> durable spool: accepted batch Ogg/Opus until terminal cleanup
```

The Discord process never receives a provider key. The gateway reads the machine token, PostgreSQL
URL, and provider keys from mounted files. Secret contents are not accepted through ordinary
environment variables, command arguments, health responses, metrics, or logs.

For portable non-root Compose custody, source secret files remain owned by the Compose user at mode
`0600`. The no-network `secret-init` container installs only the service token and selected provider
keys into a private named volume as UID/GID `10001` and mode `0400`, exits, and the gateway mounts
the result read-only. Its temporary root and `CHOWN`/`DAC_OVERRIDE`/`FOWNER` capabilities are
limited to this copy step; the long-running gateway has no capabilities. Direct bind mounts require a verified
host/container UID mapping or a narrow POSIX ACL and are not the portable default. See
[`deploy/secrets/README.md`](../../deploy/secrets/README.md).

Batch input is authoritative derived evidence from the retained recording. The gateway durably
spools accepted batch audio so recovery can finish a known-safe pre-egress job. A submitting job
interrupted without a proven provider outcome becomes terminally unknown and is never submitted a
second time. Live PCM/Opus is bounded in memory and is not written to the batch spool or database.

Provider keys and the machine token are never persisted. PostgreSQL stores normalized results and a
bounded provider reference, not provider response bodies. Operators own spool retention and backup
policy; do not treat the spool as a replacement for the original Craig recording.

## Operational limits

| Limit | Default or invariant |
| --- | --- |
| Batch upload | 64 MiB; configurable from 1 MiB through 64 MiB |
| Concurrent inbound connections | 128; configurable from 1 through 10,000 |
| Live client frame | 64 KiB gateway bound |
| Raw Discord Opus packet | at most 1,275 bytes, mono 48 kHz |
| Unacknowledged live sequences | at most 256 |
| Finalize drain | 5 seconds; configurable from 250 ms through 30 seconds |
| Provider success body | at most 16 MiB |
| Provider error body | consumed through an 8 KiB bound and never exposed |
| Keyterms | at most 100 at the public boundary; provider adapters may narrow deterministically |
| Normalized batch segments | at most 10,000 |

Capacity rejection happens before accepting additional work. Missing or mismatched profiles fail
closed; the gateway never changes providers automatically. Tune connection and upload limits only
after measuring memory, provider concurrency, PostgreSQL capacity, and spool disk usage.

## Health and observability

- `/health/live` proves the process is serving.
- `/health/ready` proves PostgreSQL and a writable spool are available on the internal gateway network.
- `/health` projects the exact configured VoiceText profiles expected by Meeting Platform on the internal gateway network.
- `/metrics` uses fixed-cardinality labels and contains no job, transcript, request, or secret
  values. The supplied TLS overlays keep it internal; only cheap `/health/live` is public.

The container probe reads `VOICETEXT_HEALTHCHECK_URL` and defaults to the internal
`http://127.0.0.1:8080/health/ready` endpoint. When `VOICETEXT_BIND_ADDR` changes the internal
address or port, change the probe URL in lockstep; Compose passes both from `deploy/.env`. A host
port mapping is irrelevant inside the container. A custom internal port also needs a matching
Compose port mapping and Caddy upstream overlay. Prefer a loopback probe address and never put
credentials in the URL.

Readiness does not perform a paid provider request. Qualify credentials with the explicit canary
workflow and synthetic audio. Alert on readiness loss, connection saturation, provider failure
classes, outcome-unknown jobs, finalize timeouts, PostgreSQL errors, and spool capacity.

## Troubleshooting

### Startup fails before listening

Check that every required path is absolute, points to a regular mounted file or durable directory,
and is readable by container UID/GID `10001`. At least one provider key path must be configured.
Configuration errors name only the variable, never the rejected value.

### `/health/live` succeeds but `/health/ready` fails

Verify PostgreSQL connectivity and migrations, then confirm the spool volume exists, has free space,
and permits create/fsync/remove for UID/GID `10001`. Do not delete accepted spool files to make the
probe green.

### Meeting Platform rejects profile readiness

Compare `VOICETEXT_BATCH_PROFILE` and `VOICETEXT_LIVE_PROFILE` with `/health`. Provider key mounts
enable exact profile families. A mixed Deepgram/ElevenLabs selection requires both keys. Historical
Deepgram batch jobs require Deepgram to remain configured until they drain.

### WebSocket never becomes ready

Confirm the first client frame is protocol-v2 config and its provider/model pair is exact. Verify
outbound WSS access and provider authorization without printing the key. TLS termination must keep
WebSocket upgrade headers and route `/api/v1/transcribe/stream` to the gateway.

For ElevenLabs live sessions, `ELEVENLABS_LIVE_AUTH_ERROR` means the mounted key is invalid or is
not authorized for realtime speech-to-text. `ELEVENLABS_LIVE_QUOTA_EXCEEDED` means the credential
reached its provider quota. Replace or re-authorize the key for the first case; top up or wait for
the provider reset for the second. Do not blindly retry a paid canary under the same campaign or
session identity. Logs intentionally retain only the bounded code and failure class, never the raw
provider error text.

### Opus frames are rejected or ACKs stop

Discord input is raw Opus packets, not RTP, Ogg, base64, or JSON. It must be mono 48 kHz and each
packet must fit the Opus bound. The client must wait for ACK-driven pacing and finalize only after
all accepted sequences are acknowledged. A missing ACK means the bounded provider write did not
complete; do not resend under the same session blindly.

### Finalize returns `timeout`

The authoritative batch path remains valid. Check provider connectivity and late transcript events.
Increase `VOICETEXT_FINALIZE_TIMEOUT_MS` only within its bound and only with latency evidence.
`flushed` is valid only after an actual provider result; the gateway will not synthesize one.

Graceful shutdown immediately makes readiness fail and stops new batch/live admission. The default
`VOICETEXT_SHUTDOWN_DRAIN_TIMEOUT_MS=245000` allows the one-minute upload deadline, three-minute
batch provider deadline, and cleanup; Compose grants 250 seconds for the same bounded sequence.
Only tasks still running at that deadline are aborted and recovered fail-closed on restart.

### Batch job is `outcome_unknown`

Do not retry or switch providers under the same idempotency key. Retain the source recording, job
identity, provider reference, and logs, then reconcile manually. A new provider effect requires an
explicit new job identity and operator decision.

### Release authorization is refused

Confirm the workflow is running in exactly `777genius/voicetext-gateway` and that the applicable
`canary-approval` or `release-publication` environment has a nonempty required-reviewer rule with
self-review prevention enabled. The run must contain exactly one unambiguous approved review for
that environment by a human other than the workflow actor. API errors, missing or changed
environment IDs, rejected or bot reviews, and empty review history deliberately stop the job before
approval evidence or publication effects.

Do not rerun a failed attempt: GitHub exposes review history for the run but cannot bind it to an
individual attempt, so the guard refuses every attempt after the first. Start a new run and obtain a
fresh environment review. Disable administrator bypass in repository settings and audit it there;
the documented environment REST response has no administrator-bypass field for the guard to check.

### Timestamps or transcript shape are rejected

Check that the selected provider/model identity matches the job, timestamps are finite, monotonic,
and within the authoritative duration, and readable segments reference existing raw segments. A
malformed provider success is deliberately classified as unknown after send.

## Backup, upgrade, and rollback

Back up PostgreSQL and the spool as one operational unit, while retaining the original Craig
recording separately. During upgrade, keep the previous image digest and exact configuration. Do
not roll back by mutating in-flight job identity or switching a live session. New sessions may use a
previous known-good image/profile after readiness is proven.

Migration `0002_exact_result_representation` is an expand-only rollback boundary: it keeps the
legacy `result_json jsonb` column, adds and backfills `result_text`, and the current gateway writes
both. To roll back during this compatibility window, stop and drain the current gateway, deploy the
previous image against the unchanged schema, and retain both columns. The previous image continues
to read and write `result_json`; if it writes while rolled back, a later current gateway prefers the
changed legacy value over stale exact text and re-synchronizes both on its next update. Do not drop
or change either column until a separately reviewed contract migration closes the rollback window.

For a clean shutdown, let the container receive `SIGTERM` and wait for its bounded graceful drain.
After restart, inspect recovery metrics and outcome-unknown jobs before launching new paid canaries.

Private SaaS adoption and rollback require the evidence bundle and stop/drain/restore criteria in
the [`release acceptance checklist`](../security/release-acceptance.md). A green deterministic test
suite alone does not authorize adoption.
