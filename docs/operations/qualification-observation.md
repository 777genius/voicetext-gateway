# Native qualification observations

The gateway can emit bounded internal evidence for an explicitly orchestrated synthetic provider
qualification. This facility is disabled by default. It does not run a canary, select credentials,
evaluate acoustics, contact Discord, or declare a release/campaign result.

## Opt in

Create an empty directory owned by the gateway process with mode `0700`, then set both:

```text
VOICETEXT_QUALIFICATION_OBSERVATION_DIR=/absolute/private/empty/directory
VOICETEXT_QUALIFICATION_CAMPAIGN=synthetic_2026_09_04
```

The campaign is 1–64 ASCII letters, digits, hyphens, or underscores. Supplying only one setting,
a relative path, a symlink destination, a directory owned by another user, or group/other mode bits
fails composition. The sink writes at most 64 create-only mode-`0600` JSON files per process. Names
are generated only from the validated campaign, mode, and a gateway UUID. Existing names are never
overwritten.

Sink failures increment `voicetext_qualification_observation_failures_total` and log only a bounded
machine code. They do not retry a provider effect and do not change the authoritative batch result,
live terminal message, or recording custody. A missing or incomplete record means qualification
evidence is missing; total disk failure cannot be rolled back by this facility.
Each sink call has a one-second gateway-side deadline; a timeout is reported as
`QUALIFICATION_WRITE_TIMEOUT` and the authoritative operation continues unchanged.
If writing or syncing a newly created record fails, the sink makes a bounded best-effort removal of
that partial file and still reports the original machine-readable failure.

## Records

Every file uses schema `voicetext-qualification-observation-v1`. `effect_id` is a gateway effect
identity and is deliberately separate from `provider_operation`. A provider operation is present
only when the adapter observed an actual native identity:

- `request_id`: Deepgram batch/live request IDs, or an ElevenLabs HTTP request fallback;
- `transcription_id`: an ElevenLabs batch transcription ID;
- `session_id`: an ElevenLabs live session ID.

An absent identity remains JSON `null` and is unqualified. IDs are never invented and their kind is
never inferred from opaque bytes. Batch records bind gateway job ID, immutable profile, observed
provider operation, observed normalized-result digest, millisecond start/finish times, terminal
state, and whether durable persistence was actually established. A persisted idempotent replay has
no provider effect and therefore creates no observation. `not_established` is never a durable
success, even when a provider result was observed.

Live records bind the VoiceText `client_session_id`, the exact gateway `ready.session_id`, immutable
profile, latest actual provider operation, accepted-frame count, successful provider-write range,
successfully delivered ACK range, raw-input digest, normalized-result digest, finalize-result flag,
terminal status, and millisecond times. Sequence summaries contain count, first, last, and a
contiguous flag; they remain bounded for a four-hour session. Failed provider writes are absent from
the written and ACK summaries. Failed client ACK deliveries are absent from the ACK summary and
raw-input digest. No audio or transcript text is stored in a record.

### Raw acknowledged-frame digest

Initialize SHA-256 with ASCII `voicetext-acked-frames-v1\n`. For every successfully ACK-sent frame,
in ACK order, append the sequence as unsigned 64-bit big-endian, raw payload length as unsigned
64-bit big-endian, and the exact binary VoiceText input payload received before Opus decoding. The
lowercase hexadecimal SHA-256 is `acked_raw_input_digest`. Length prefixes prevent frame-boundary
collisions; decoded PCM produces a different digest from raw Opus.

### Normalized-result digest

Version 1 initializes SHA-256 with ASCII `voicetext-normalized-result-v1\n`. Every string is encoded
as unsigned 64-bit big-endian byte length followed by UTF-8 bytes. Integers use unsigned big-endian.
Optional duration uses `u64::MAX` when absent; optional confidence uses the IEEE-754 `f32` bits or
`u32::MAX` when absent.

For batch, append provider, model, language, normalized full text, contract version, authoritative
duration, provider duration, segment count, then each segment's start, end, text, confidence, and
speaker (empty bytes when absent), in normalized order. For live, append each observed event in
order: `transcript\0` plus text/start/duration/confidence/stability discriminant;
`utterance_end\0` plus last-word end; or `finalize_result\0`. This digest describes what the gateway
actually normalized, not a provider response body.

## Canary producer consumption

The later producer should start from an empty private directory, launch the shipped gateway with a
unique validated campaign, drive its separately approved synthetic fixture, stop the gateway, and
read create-only files. It must match planned effects by mode, profile, gateway job/session IDs, and
typed native operation—not invent a `provider_result_id`. It should reject missing/null required
identity, non-contiguous expected ACKs, non-established batch persistence, unexpected terminal or
finalize evidence, duplicate effect IDs, count drift, digest mismatch, extra files, or sink-failure
metrics. Source/image/runner/credential-owner/fixture provenance and acoustic scoring must be joined
by that producer. These observations alone are never a campaign or release PASS.
