# VoiceText Gateway

Provider-agnostic Rust speech-to-text library and self-hostable gateway for Discord Meeting
Assistant. It preserves the current VoiceText HTTP/WebSocket contract while removing any runtime
dependency on the private VoiceText SaaS.

Users run the gateway with their own provider keys. Discord talks to it through one machine bearer
token; provider keys never enter the bot container.

## Status

The runnable vertical slice is implemented and deterministic provider/audio suites pass locally:

- Deepgram Nova-3 batch and live;
- ElevenLabs Scribe v2 batch and Scribe v2 Realtime live;
- batch contracts v2/v3 and live protocol v2;
- raw Discord mono 48 kHz Opus and legacy PCM S16LE 16 kHz live input;
- complete single-stream mono or stereo Ogg Opus batch input;
- durable PostgreSQL idempotency, content-addressed audio spool, CAS submission fence and restart
  recovery;
- machine authentication, health/readiness, fixed-cardinality metrics and graceful listener
  shutdown;
- non-root Docker image and standalone Compose deployment.

Real paid-provider canaries and private test-guild Discord E2E remain opt-in publication gates. No
production provider request is made by the normal test suite.

## Provider-agnostic means

The domain and application layers know no provider SDK or wire type. Batch and live STT are separate
consumer-owned ports; adapters implement only capabilities they actually support. Composition binds
an exact provider/model before a job or session starts, and automatic provider fallback is forbidden.

Deepgram and ElevenLabs are the native qualified adapters in V1. A new provider can be added without
changing Discord or the domain state machines, but must pass the same conformance suite.

Pipecat is intentionally not included. A future optional sidecar adapter is documented in
[`docs/extensions/pipecat.md`](docs/extensions/pipecat.md) and must pass conformance before it can be
advertised as supported.

## Language support

There are two separate meanings of language support:

- Discord presentation/localization currently formats final summaries and transcripts in English,
  Russian and Ukrainian.
- Speech recognition languages are provider capabilities. Live requests accept safe explicit codes
  such as `ru`, `en-US` and `pt-BR`, or `multi`. Batch V1 uses multilingual provider profiles.

Therefore the project is not limited to three recognition languages. Actual quality and availability
depend on the selected provider/model; the gateway does not claim an untested universal language
matrix.

See the [`provider capability matrix`](docs/provider-capability-matrix.md) for the exact batch/live
formats and the distinction between implemented routing, deterministic conformance, and paid
English/Russian provider qualification.

## Quick start with Docker Compose

Requirements: Docker Compose, one Deepgram and/or ElevenLabs API key, and a machine with persistent
storage.

1. Copy `deploy/.env.example` to `deploy/.env`.
2. Create the files listed in [`deploy/secrets/README.md`](deploy/secrets/README.md), then set their
   permissions to `0600`.
3. Start the private HTTP/WS gateway:

   ```bash
   docker compose --env-file deploy/.env -f deploy/compose.yaml up --build -d
   ```

4. For the Discord bot, expose a trusted WSS endpoint. Point a DNS record at the host, set
   `VOICETEXT_PUBLIC_HOST`, then start the optional Caddy TLS overlay:

   ```bash
   docker compose --env-file deploy/.env \
     -f deploy/compose.yaml -f deploy/compose.tls.yaml up --build -d
   ```

5. Configure Discord Meeting Assistant with:

   ```text
   VOICETEXT_WS_URL=wss://voice.example.com/api/v1/transcribe/stream
   VOICETEXT_TOKEN=<same contents as deploy/secrets/gateway_token>
   ```

The batch client derives `https://voice.example.com/api/v1/transcribe/batch` from the WSS URL.

The default Compose file enables both providers. For a single-provider deployment, add the matching
override and create only that provider's key file:

```bash
# Deepgram only
docker compose --env-file deploy/.env \
  -f deploy/compose.yaml -f deploy/compose.deepgram.yaml up --build -d

# ElevenLabs only
docker compose --env-file deploy/.env \
  -f deploy/compose.yaml -f deploy/compose.elevenlabs.yaml up --build -d
```

The same override can be combined with `deploy/compose.tls.yaml`. Missing profiles fail explicitly;
they never fall back to another provider.

## Runtime configuration

Required file/path variables:

| Variable | Purpose |
| --- | --- |
| `VOICETEXT_POSTGRES_URL_FILE` | Mounted PostgreSQL URL secret |
| `VOICETEXT_BEARER_TOKEN_FILE` | Mounted Discord-to-gateway token |
| `VOICETEXT_SPOOL_DIR` | Durable accepted-audio directory |

Provider secrets are optional individually:

- `VOICETEXT_DEEPGRAM_API_KEY_FILE`
- `VOICETEXT_ELEVENLABS_API_KEY_FILE`

Operational settings include `VOICETEXT_BIND_ADDR`, `VOICETEXT_FINALIZE_TIMEOUT_MS`,
`VOICETEXT_MAX_CONNECTIONS`, `VOICETEXT_MAX_UPLOAD_BYTES` and the four documented provider endpoint
overrides. Provider endpoints require HTTPS/WSS. Plain HTTP/WS is accepted only with the explicit
local-test flag `VOICETEXT_ALLOW_INSECURE_PROVIDER_ENDPOINTS=true`.

Endpoints:

- `POST /api/v1/transcribe/batch`
- `GET /api/v1/transcribe/batch/{job_id}`
- `GET /api/v1/transcribe/stream` (WebSocket upgrade)
- `GET /health/live`
- `GET /health/ready`
- `GET /health` (current VoiceText provider-profile compatibility projection)
- `GET /metrics` (internal gateway network; the TLS overlays do not publish it)

## Architecture

```text
voicetext-speech       deterministic domain + application-owned ports
        ^
voicetext-providers    native Deepgram/ElevenLabs outbound adapters
        ^
voicetext-gateway      HTTP/WS, auth, PostgreSQL, spool, operations, composition
```

Batch recording is authoritative evidence. Live transcription is a derived best-effort projection.
An uncertain paid submission is terminally fenced and never retried automatically. Provider or
summary failure cannot delete the source recording.

See [`docs/architecture/overview.md`](docs/architecture/overview.md),
[`docs/contracts/voicetext-compatibility.md`](docs/contracts/voicetext-compatibility.md) and the
accepted decisions in [`docs/decisions`](docs/decisions).

For extension and operations, see the
[`provider adapter authoring guide`](docs/extensions/provider-adapter.md) and
[`self-hosting/security runbook`](docs/operations/self-hosting.md).

## Verification

```bash
cargo xtask verify
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --no-deps
cargo deny check
```

The ignored PostgreSQL integration test requires a new disposable test database. Real provider and
Discord tests require test-only keys, synthetic audio, an official bot and a private test guild.
Never use a user account, self-bot, public guild, real meeting or customer recording.

## Non-goals

TTS, LLM orchestration, summaries, billing, public accounts, OAuth, licenses and the private SaaS
product stay outside this STT bounded context.

## Publication and license

The intended OSS license is `MIT OR Apache-2.0`. Publishing remains disabled until the repository
owner confirms the license and the provenance, secret, dependency, SBOM and real-canary gates pass.
The private SaaS Git history is never imported.
