# Provider adapter authoring guide

Add a provider only when one real batch or live vertical slice is ready. Do not add empty provider
folders, generic SDK wrappers, or capabilities the provider cannot prove.

## Boundary to implement

Provider code belongs in `voicetext-providers` and depends inward on the consumer-owned ports from
`voicetext-speech`:

- batch implements `BatchRecognizer`;
- live composition implements `LiveRecognizerFactory`;
- one open live connection implements `LiveRecognizerSession`.

The adapter receives an exact immutable profile. It must reject a mismatched contract version,
provider, model, or language before provider egress. Provider SDK, HTTP, WebSocket, credentials,
wire DTOs, clocks, and retry headers must not enter `voicetext-speech`.

## Failure mapping

Every egress failure must be mapped to one of the public `RecognitionFailure` variants:

- `KnownNotAccepted { retryable: true }` only when there is evidence that the provider did not
  accept the effect and a new submission is safe;
- `KnownNotAccepted { retryable: false }` for local validation or a proven terminal rejection;
- `KnownAcceptedTerminal` when provider acceptance is proven but the result is terminal;
- `UnknownAfterSend` whenever the request may have crossed the provider boundary.

Network errors, response truncation, malformed success responses, timeouts after egress, and
unclassified status codes default to `UnknownAfterSend`. Never retry or fall back to another
provider inside an adapter. Error codes and provider references are bounded non-secret evidence;
raw provider bodies never cross the port.

## Batch checklist

1. Validate the exact profile and bounded input before building the request.
2. Send one paid request and read success/error bodies through explicit byte limits.
3. Normalize text, raw segments, readable segments, confidence, speaker, timestamps, provider
   duration, and request reference into `BatchRecognitionResult`.
4. Use the locally verified recording duration as `duration_millis`; provider duration remains
   diagnostic.
5. Reject non-monotonic, negative, overflowing, or out-of-authority timelines.
6. Keep repeated keyterms only when the provider supports them; document deterministic adaptation
   when provider limits are narrower than the gateway contract.

## Live checklist

1. Open only the requested profile and declare readiness only after the provider connection is
   usable.
2. Complete `write_audio` only after the bounded provider write succeeds. The gateway sends the
   client ACK after this return.
3. Normalize provider events to partial, segment-final, utterance-final, utterance-end, and
   `FinalizeResultObserved` without inventing a final event.
4. `finalize` initiates provider-specific flushing. It does not by itself prove that a result was
   observed.
5. `close` is bounded and idempotent from the caller's perspective.
6. Preserve monotonic millisecond timestamps and bound ignored/control messages so a provider
   cannot starve the event loop.

## Gateway exposure

After the adapter tests pass, add a closed identity to the batch or live contract registry, register
the concrete adapter in composition, expose its readiness capability, and extend the independent
TypeScript consumer fixtures. Batch and live identities remain separate. A provider may implement
only one mode.

Do not change existing profile semantics to fit a new provider. A wire-contract change requires a
new version and compatibility fixtures.

## Conformance gates

The new adapter must add deterministic local wire fakes covering request shape, response parsing,
body/frame limits, every failure class, timestamps, keyterms, provider identity, redaction, and
finalize ordering. Then run:

```sh
cargo xtask verify
cargo test -p voicetext-gateway --test black_box_conformance
DISCORD_MEETING_ASSISTANT_ROOT=/absolute/path/to/discord-voice-bot \
  cargo test -p voicetext-gateway \
  checked_in_typescript_consumer_matches_the_real_gateway -- --ignored --exact
```

The production-composition gate from the testing strategy must pass before a release. A real paid
canary uses a test-only key, synthetic audio, one planned effect, a fresh identity, and retained
non-secret evidence. Provider documentation alone is not qualification evidence.

