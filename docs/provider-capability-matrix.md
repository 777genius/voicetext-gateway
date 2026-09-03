# Provider capability matrix

This matrix describes capabilities exposed by the current VoiceText Gateway adapters. It is not an
exhaustive list of features advertised by either provider. A provider or model may support more than
the gateway requests, normalizes, and exposes through its versioned contracts.

## Qualification terms

- **Implemented** means the Rust adapter and gateway composition exist.
- **Conformance-tested** means deterministic contract, adapter-wire, and gateway tests cover the
  capability with synthetic audio and local provider fakes. These tests make no paid request and do
  not qualify real-provider behavior or recognition quality.
- **Real-provider-qualified** requires a retained, test-only canary receipt tied to the exact source
  revision and image digest, provider, model, mode, language selection, and synthetic fixture.
  Provider documentation, an accepted language code, or a fake-provider test is not qualification
  evidence.

All four profiles below are implemented and conformance-tested. This repository does not contain
retained real-provider canary receipts tied to an exact revision that qualify English or Russian
recognition quality for any row. The canaries remain opt-in publication gates. Accordingly, `multi`,
`en`, `en-US`, and `ru` below describe configured or accepted routing, not a universal
language-quality claim.

The exact mapping from each claim to checked-in conformance tests and the additional bounded canary
record required for publication is maintained in the
[`release acceptance checklist`](security/release-acceptance.md). Missing real-provider evidence
means “unqualified,” never “implicitly supported.”

## Current provider adapters

| Provider / model | Mode and contract | Gateway input | Timestamps exposed | Partials and finals | Finalize | Keyterms | Diarization / speakers | Language selection and qualification |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Deepgram Nova-3 | Batch, contract v2 | Complete, single-stream Ogg Opus (`audio/ogg`), mono or stereo | **Provider-native:** provider utterance and paragraph timing is validated, normalized, and returned as timestamped utterances/readable segments | Completed result only | Not applicable to batch | Repeated `keyterm` query values are forwarded | Not requested; speaker labels are not exposed by the v2 gateway response | Gateway exposes only the `multi` profile. English/Russian acoustic quality is not qualified by checked-in real-provider evidence. |
| ElevenLabs Scribe v2 | Batch, contract v3 | Complete, single-stream Ogg Opus (`audio/ogg`), mono or stereo | **Provider-native:** word timestamps are requested, validated against authoritative recording duration, and normalized into timestamped segments | Completed result only | Not applicable to batch | Gateway keyterms are validated, normalized, deduplicated, sorted, and sent as repeated fields | Explicitly disabled; speaker labels are not exposed | Gateway exposes only the `multi` profile. English/Russian acoustic quality is not qualified by checked-in real-provider evidence. |
| Deepgram Nova-3 | Live, protocol v2 | Binary raw Discord mono 48 kHz Opus packets, or legacy mono PCM S16LE at 16 kHz; Opus is decoded to mono PCM S16LE before provider egress | **Provider-native:** provider `start` and `duration` are normalized to gateway millisecond segments; utterance-end timing is preserved, then checked by the shared accepted-audio policy | Interim results map to partials; provider final/speech-final results map to immutable segment/utterance finals governed by the shared non-overlap policy | Sends provider `Finalize`; gateway reports `flushed` only after observing a finalize result, otherwise bounded `timeout` (`no_provider` only when no audio was accepted) | Trimmed, bounded values are sent as repeated `keyterm` query values | Not requested or exposed | Adapter accepts safe explicit codes such as `en-US` and `ru`, plus `multi`. These routes have deterministic validation/wire coverage, not checked-in real-provider English/Russian qualification. |
| ElevenLabs Scribe v2 Realtime | Live, protocol v2 | Binary raw Discord mono 48 kHz Opus packets, or legacy mono PCM S16LE at 16 kHz; Opus is decoded to mono PCM S16LE before provider egress | **Gateway-synthesized:** the adapter requests no provider timestamps and derives a monotonic millisecond timeline from accepted PCM duration; optional word-end evidence never changes that provenance. The shared accepted-audio policy independently checks every exposed timestamp. | Provider partial, final, and committed messages map to partial, segment-final, and immutable utterance-final events governed by the shared non-overlap policy | Sends an explicit empty commit; gateway reports `flushed` only after observing the committed finalize result, otherwise bounded `timeout` (`no_provider` only when no audio was accepted) | Values are whitespace-normalized and deduplicated; at most 50 terms of at most 20 Unicode scalar values are accepted and forwarded | Not requested or exposed | `multi` enables provider language detection; safe explicit codes such as `en-US` and `ru` are forwarded as `language_code`. These routes have deterministic validation/wire coverage, not checked-in real-provider English/Russian qualification. |

## Shared contract limits

- Batch input is authoritative evidence. Live output is a derived, best-effort projection.
- Live audio is always mono at provider egress: linear PCM S16LE at 48 kHz or 16 kHz.
- Live ACK is emitted only after the corresponding provider write succeeds. Finalize starts only
  after every accepted frame has an ACK.
- The live application derives a monotonic accepted-audio horizon only from successfully written,
  exactly channel-aligned PCM S16LE frames: `floor(total sample frames * 1000 / sample rate)`.
  Transcript ends and utterance-end timestamps may lead that horizon by at most 250 milliseconds
  (inclusive), accommodating bounded provider segmentation. Greater lead and arithmetic overflow
  are explicit invalid-provider-event failures; timestamps are never silently clamped.
- Final transcript intervals use a provider-neutral half-open ordering policy: every segment-final
  or utterance-final start must be at or after the preceding final end. Final regression and
  overlap are rejected. Partial events may revise or overlap prior partial/final timing, but remain
  subject to the accepted-audio horizon.
- Batch and live profiles are bound independently. There is no automatic provider fallback.
- Public JSON text bounds use UTF-16 code units to match the TypeScript consumer. Provider limits
  use their documented unit explicitly (currently Unicode scalar values for ElevenLabs keyterms).
  The checked descriptor enforces both the 64 MiB/64 KiB public bounds and any distinct upstream
  provider bound before egress.
- The current VoiceText response contracts do not expose word-level tokens or speaker labels, even
  when a provider response contains additional evidence.

## Pipecat

Pipecat is future, documentation-only work. There is no Pipecat dependency, sidecar, adapter,
runtime path, or qualified capability in V1. A future adapter must pass the same batch/live
separation, input normalization, language, timestamp, partial/final, keyterm, and truthful-finalize
conformance gates before any row can be added here as supported.
