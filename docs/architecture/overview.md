# Architecture overview

VoiceText Gateway is one Speech Transcription bounded context with two feature slices: authoritative
batch transcription and derived live transcription. Strategic DDD names the real invariants;
transport and provider mechanics remain hexagonal adapters.

```text
voicetext-speech
  domain <- application ports
                    ^
                    |
voicetext-providers adapters
                    ^
                    |
voicetext-gateway inbound/outbound adapters <- composition
```

The reusable library owns deterministic batch and live state machines plus narrow consumer-owned
ports. Provider adapters implement those ports. The runnable gateway owns VoiceText wire contracts,
machine authentication, PostgreSQL persistence, the bounded audio spool, health, metrics, and
dependency selection.

The private VoiceText SaaS will consume the same public library and provider adapters while keeping
its own users, licensing, quota, billing, and PostgreSQL composition private.

## Evidence roles

- Batch transcription derives from a complete authoritative recording artifact and is durable.
- Live transcription is a bounded, best-effort projection and cannot replace batch evidence.
- Provider failure never deletes or invalidates the source recording.
- A bound provider/model identity is immutable for one batch job or live session.
