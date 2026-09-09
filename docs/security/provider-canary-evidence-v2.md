# Provider canary evidence schema v2

This document defines the bounded campaign and canary records accepted for new provider
qualification. It changes evidence representation only; it does not change a VoiceText protocol,
provider adapter, runtime effect, approval identity, or signing scheme. Version 1 campaign and
canary records remain historical artifacts and are not rewritten, but they are rejected for new
qualification.

The four record versions are deliberately independent:

- `campaign-manifest.json`: schema version 2;
- `provider-canary.json`: schema version 2;
- `fixture-manifest.json`: unchanged schema version 1;
- `reviewer-approval.json`: unchanged schema version 1. Its `approved_at` may use whole UTC seconds
  or exactly three fractional digits; v2 campaign/canary observation times require the latter.

All records remain strict canonical JSON with no unknown or duplicate keys. The four protected
records remain limited to 64 KiB each. Digests use `sha256:` plus 64 lowercase hexadecimal digits;
SHA bindings use 64 lowercase hexadecimal digits without a prefix.

## Provider operation identity

Every campaign effect has exactly one nonempty `provider_operation` object:

```json
{"id":"opaque-provider-value","kind":"deepgram_request_id"}
```

The allowlist is:

| Provider | Mode | Allowed `kind` |
| --- | --- | --- |
| Deepgram | batch | `deepgram_request_id` |
| Deepgram | live | `deepgram_request_id` |
| ElevenLabs | batch | `elevenlabs_transcription_id`, or `elevenlabs_http_request_id` only when the transcription identifier is unavailable |
| ElevenLabs | live | `elevenlabs_session_id`, or `elevenlabs_http_request_id` only when the session identifier is unavailable |

The opaque identifier is 1 through 128 printable ASCII characters. These are observed provider
identifiers. An HTTP fallback is an identifier actually returned by
the provider for that HTTP operation, not a gateway-generated request identifier. Missing native
evidence fails qualification. A producer must not prefix, suffix, hash, or otherwise transform an
identifier to manufacture another provider operation.

Provider operations are unique across all standalone and mixed effects by `(actual provider,
opaque id)`. The `kind` label is not part of this replay key, so relabeling cannot hide reuse. The
gateway `effect_id` is a separate correlation namespace and is globally unique among the eight
effects. It may have the same bytes as an unrelated provider's opaque identifier.

`result_digest` is the SHA-256 digest of the observed normalized transcription output. It is not a
provider result identifier and cannot substitute for `provider_operation`.

## Campaign manifest

The version 2 campaign manifest has exactly these top-level keys:

```text
schema_version, source_sha, image_digest, campaign_id, runner, effects
```

It contains exactly eight fresh effects covering all six required profiles. Each effect has
exactly:

```text
position, profile, kind, fixture_id, fixture_digest, result_digest,
effect_id, provider_operation
```

The profile and effect kind determine the allowed provider operation kind. Fixture identifiers and
digests must be present in the unchanged version 1 fixture manifest.

## Canary record and effect proofs

The version 2 canary has the same top-level fields as version 1, with `schema_version` set to 2. It
contains all six profiles and binds exactly the campaign's eight effect positions. Every effect
copies its campaign `fixture_id`, `fixture_digest`, `result_digest`, `effect_id`, and complete
`provider_operation` without modification, and adds the exact provider/model/mode/contract/language
identity, outcome, error classification, and timing evidence.

`started_at`, `completed_at`, terminal/finalize times, and the canary `completed_at` use unambiguous
UTC `YYYY-MM-DDTHH:MM:SS.mmmZ`. Exactly three fractional digits are required. `latency_ms` is a
non-negative JSON integer equal to the exact millisecond difference between the effect timestamps;
reversed bounds, booleans, non-finite values, rounding, and truncation are invalid. Terminal times
must fall within their effect, and every effect must complete no later than the canary.

A batch `provider_terminal` contains exactly:

```text
status="completed", provider_operation, effect_id, result_digest, observed_at
```

A live effect retains `accepted_frame_count`, `accepted_frames_digest`, `ack_first`, `ack_last`, and
has a `finalize` containing exactly:

```text
status="flushed", provider_operation, effect_id, result_digest, terminal_at
```

Both terminal objects copy the enclosing effect's operation, correlation ID, and normalized-output
digest exactly. Live evidence requires a positive accepted-frame count and the complete inclusive
ACK range `1..accepted_frame_count`.

## ACKed-frame digest recipe for a future producer

The versioned recipe name is `voicetext-acked-frames-v1`. Begin a SHA-256 stream with the exact
ASCII bytes `voicetext-acked-frames-v1` followed by one newline byte (0x0a). For every accepted frame in
strict sequence order, after its corresponding ACK is observed, append:

1. the sequence number as an unsigned 64-bit big-endian integer;
2. the frame byte length as an unsigned 64-bit big-endian integer;
3. the exact accepted frame bytes, without decoding or re-encoding.

The evidence value is `sha256:` followed by the lowercase hexadecimal SHA-256 result. Sequence
numbers begin at 1, are contiguous, and end at `accepted_frame_count`; a length greater than
the gateway's configured input-frame bound is invalid. These are the raw VoiceText binary input
payloads before Opus decoding, not reconstructed PCM. This length-delimited recipe prevents concatenation ambiguity. It defines the
next producer slice but does not claim that a producer currently implements it.

## What structural verification does not prove

The verifier establishes shape, identity allowlists, exact bindings, freshness/replay constraints,
timestamps, ACK range, and terminal consistency. It does not authenticate the canary producer's
origin by itself; the unchanged protected approval and GitHub OIDC attestation checks remain
separate. It also does not measure acoustic WER/CER, terminology accuracy, or transcript/timeline
quality. Authenticated producer output and those quality qualifications remain uncompleted
producer/qualification requirements and cannot be claimed from structural acceptance alone.
