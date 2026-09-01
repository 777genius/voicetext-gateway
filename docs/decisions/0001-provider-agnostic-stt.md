# ADR-0001: Provider-agnostic STT library and runnable gateway

Status: accepted

## Decision

Build an STT-only reusable Rust library and a runnable VoiceText-compatible gateway. Batch and live
capabilities have separate ports. Deepgram and ElevenLabs are outbound adapters selected only in
composition. Provider/model identity is immutable after job or session binding, and provider
fallback is forbidden.

The public gateway contains machine authentication and operational persistence, not the private
SaaS user, license, quota, billing, or payment model.

## Consequences

Discord Meeting Assistant can self-host the gateway with user-owned provider credentials. The
private SaaS can reuse the same provider mechanics without becoming an OSS dependency.

