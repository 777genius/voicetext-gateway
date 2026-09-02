# Testing strategy

## Deterministic layers

- Domain tests exhaust every batch and live transition without mocks, network, clock, database, or
  runtime.
- Application tests use in-memory ports and prove idempotency, cancellation, provider binding,
  retry classification, and unknown-outcome fencing.
- Live application tests prove exact PCM S16LE frame alignment, checked accepted-audio horizon
  arithmetic, the inclusive 250 ms provider-lead boundary, future-event rejection, non-overlapping
  final ordering, and unrestricted partial revision for both timestamp provenance modes.
- Provider adapter tests use local HTTP/WebSocket fakes and bounded synthetic fixtures. They verify
  provider request construction and response projection without paid traffic.
- Gateway contract tests run the real HTTP/WebSocket server and the independent Discord Meeting
  Assistant TypeScript client against both fake providers.
- Restart tests use disposable PostgreSQL and spool storage to reconstruct pre-egress accepted,
  interrupted submitting, and already completed states without repeating an uncertain provider
  effect.

## Provider qualification

Real provider canaries are opt-in and never replace deterministic tests. Each run uses a test-only
key and synthetic audio and performs exactly one planned provider effect:

1. Deepgram Nova-3 batch;
2. ElevenLabs Scribe v2 batch;
3. Deepgram Nova-3 live;
4. ElevenLabs Scribe v2 Realtime live;
5. Deepgram batch with ElevenLabs live;
6. ElevenLabs batch with Deepgram live.

The checked-in TypeScript consumer fixture is dependency-free and digest-pinned. The required
production-composition release job runs it without resolving a mutable consumer repository or npm
dependency. The older in-memory cross-repository test remains available for integration work:

```sh
DISCORD_MEETING_ASSISTANT_ROOT=/absolute/path/to/discord-voice-bot \
  cargo test -p voicetext-gateway \
  checked_in_typescript_consumer_matches_the_real_gateway -- --ignored --exact
```

This gate uses only in-memory fake providers and synthetic Ogg/Opus. It performs no paid provider or
Discord request.

Production composition is a required CI and release gate. It starts the release-mode shipped
gateway binary, reads
permission-restricted secret files, migrates a disposable PostgreSQL database, connects the real
Deepgram and ElevenLabs HTTP/WebSocket adapters to deterministic wire fakes, and drives all four
profiles plus both mixed batch/live selections through the checked-in TypeScript consumer:

```sh
VOICETEXT_TEST_DATABASE_URL=postgresql://.../voicetext_test_<unique> \
VOICETEXT_GATEWAY_PRODUCTION_BINARY="$PWD/target/release/voicetext-gateway" \
  scripts/run-production-composition-gate.sh
```

The gate verifies the fixture SHA-256 before use and asserts one provider effect per original
batch/live request and zero provider effects for idempotent batch replay. Its PostgreSQL database is
disposable, provider endpoints are loopback-only fakes, credentials and audio are synthetic, and it
performs no paid provider or Discord request. Node.js 24 or newer is required to execute the exact
TypeScript source directly.

Durable startup recovery has a separate disposable-PostgreSQL gate:

```sh
VOICETEXT_TEST_DATABASE_URL=postgresql://.../voicetext_test_<unique> \
  cargo test -p voicetext-gateway durable_startup_recovery_is_exactly_once \
  -- --ignored --exact
```

It proves that pre-egress accepted work resumes once, interrupted `submitting` work becomes
terminally unknown without a second provider call, and already persisted work remains terminal.
The production-composition script executes this exact ignored test after its provider-adapter and
TypeScript-consumer test, so the same recovery proof is required by CI and tagged releases.

The canary retains only bounded non-secret evidence: selected identity, request/result IDs, fixture
digest, result digest, timestamps, ACK counts, finalize status, latency, and error classification.

## Discord qualification

Real Discord E2E is allowed only with official bot applications, a private test guild, test-only
channels and identities, synthetic audio, and disposable storage. User accounts, self-bots, public
guilds, customer recordings, and real user projects are forbidden.

## Security and operations

- exact request and response size bounds;
- malformed multipart, JSON, WebSocket, Ogg, and Opus inputs;
- authorization timing and redaction tests;
- secret-file permissions and absence from argv, logs, health, metrics, and image layers;
- non-root, read-only, no-new-privileges container checks;
- graceful shutdown, orphan-spool cleanup, migration, readiness, and rollback tests;
- dependency license, vulnerability, SBOM, and clean-history secret scans before publication.
