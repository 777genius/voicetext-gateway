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
fails composition. The sink writes at most 64 create-only mode-`0600` JSON files per process. It
writes and syncs a private non-final temporary inode, publishes it without replacement, removes the
temporary name, and syncs the pinned directory before reporting success. Final names are generated
only from the validated campaign, mode, and a gateway UUID. Existing names are never overwritten.

Sink failures increment `voicetext_qualification_observation_failures_total` and log only a bounded
machine code. They do not retry a provider effect and do not change the authoritative batch result,
live terminal message, or recording custody. A missing or incomplete record means qualification
evidence is missing; total disk failure cannot be rolled back by this facility.
Each sink call gives its caller a one-second gateway-side deadline; the deadline is not extended.
All filesystem work and cleanup run on an isolated blocking worker. The worker checks both its own
deadline and an explicit caller receipt before and after publication. A published name remains
provisional until writing, record sync, create-only publication, temporary-name removal, directory
sync, and the caller's acceptance have all completed. Caller acceptance rechecks the same absolute
deadline while holding the receipt-state mutex. Timeout or cancellation wins that receipt, so later
worker completion cannot turn the call into qualified evidence.

Native filesystem syscalls are not preemptible and this is not a hard syscall-completion deadline.
A syscall or inode-checked best-effort cleanup can finish after the caller has reported
`QUALIFICATION_WRITE_TIMEOUT`; a provisional name can therefore be transiently visible. Cleanup
checks identity before unlinking, but check-then-unlink is not atomic against a same-UID process
that can replace names. The directory must remain private mode `0700`, and an untrusted same-UID
writer is outside the threat model. Filesystem failure, process termination, or power loss can
leave cleanup ambiguous. In particular, a provisional final-looking JSON name can survive an
abnormal process exit; this sink does not by itself close that B1 campaign-evidence gap. The
separate external campaign producer must fail closed on forced or crashed process exit, await all
blocking workers on an orderly stop, and require a clean gateway exit, zero sink-failure metrics,
and the exact complete expected file inventory before joining observations into campaign evidence.
File presence alone never proves a successful sink call. The authoritative operation continues
unchanged.

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
gateway-emitted ACK range, raw-input digest, normalized-result digest, finalize-result flag,
terminal status, and millisecond times. Sequence summaries contain count, first, last, and a
contiguous flag; they remain bounded for a four-hour session. Failed provider writes are absent from
the written and ACK summaries. Failed client ACK emissions are absent from the ACK summary and
raw-input digest. No audio or transcript text is stored in a record.

### Raw acknowledged-frame digest

Initialize SHA-256 with ASCII `voicetext-acked-frames-v1\n`. For every frame whose ACK WebSocket
send completed at the gateway, in emission order, append the sequence as unsigned 64-bit
big-endian, raw payload length as unsigned 64-bit big-endian, and the exact binary VoiceText input
payload received before Opus decoding. The lowercase hexadecimal SHA-256 is
`acked_raw_input_digest`. Length prefixes prevent frame-boundary collisions; decoded PCM produces a
different digest from raw Opus. WebSocket send completion is not authenticated peer receipt; a
producer that needs that stronger claim must independently compare client-observed ACKs.

### Normalized-result digest

Version 1 initializes SHA-256 with ASCII `voicetext-normalized-result-v1\n`. Every string is encoded
as unsigned 64-bit big-endian byte length followed by UTF-8 bytes. Contract version is unsigned
16-bit big-endian; timestamps, durations, and segment count are unsigned 64-bit big-endian.
Optional duration uses `u64::MAX` when absent; optional confidence uses the unsigned 32-bit
big-endian IEEE-754 `f32` bits or `u32::MAX` when absent.

For batch, append provider, model, language, normalized full text, contract version, authoritative
duration, provider duration, segment count, then each segment's start, end, text, confidence, and
speaker (empty bytes when absent), in normalized order. For live, append each observed event in
order: `transcript\0` plus text/start/duration/confidence and one stability byte (`0` partial, `1`
segment-final, `2` utterance-final);
`utterance_end\0` plus last-word end; or `finalize_result\0`. This digest describes what the gateway
actually normalized, not a provider response body.

## Canary producer consumption

The later producer should start from an empty private directory, launch the shipped gateway with a
unique validated campaign, drive its separately approved synthetic fixture, and then stop the
gateway. It must fail closed if the process was forced or crashed; on orderly shutdown it must await
all blocking workers before reading create-only files. It must match planned effects by mode,
profile, gateway job/session IDs, and typed native operation—not invent a `provider_result_id`. It
must require clean process exit, zero sink-failure metrics, and an exact complete file inventory,
and reject missing/null required identity, non-contiguous expected ACKs, non-established batch
persistence, unexpected terminal or finalize evidence, duplicate effect IDs, count drift, digest
mismatch, missing files, or extra files before any joins. Source/image/runner/credential-owner/
fixture provenance and acoustic scoring must be joined by that producer. These observations alone
are never a campaign or release PASS.
