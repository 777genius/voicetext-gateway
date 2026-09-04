# Repository guardrails

Read these before changing production source:

1. `docs/architecture/overview.md`
2. `docs/architecture/dependency-rules.md`
3. `docs/architecture/testing-strategy.md`
4. `docs/contracts/voicetext-compatibility.md`
5. `docs/implementation-plan.md`

## Hard rules

- Domain code is deterministic and has no framework, transport, database, provider, environment,
  wall-clock, randomness, or timer dependency.
- Application code depends only on domain code and consumer-owned ports.
- Provider and transport DTOs never enter domain or application code.
- Deepgram, ElevenLabs, PostgreSQL, Axum, and future Pipecat types remain in adapters or
  composition.
- Batch and live capabilities use separate ports. Never add a fat provider interface with
  unsupported operations.
- No automatic fallback between providers after a job or session is bound to a profile.
- Unknown outcomes after provider egress are never retried as a new paid request.
- Every production Rust source file must be classified by
  `architecture/source-dependencies.txt` and remain at or below 600 lines.
- Do not add generic `shared`, `common`, `utils`, service-locator, or universal-event modules.
- Pipecat is documentation-only until a future conformance-tested adapter is explicitly approved.
- TTS, LLM, billing, public users, OAuth, licenses, and payments are outside this repository's STT
  bounded context.

## Verification

Run while editing:

```text
cargo xtask verify
```

Before handoff, also run:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --no-deps
```

Real provider and Discord tests require test-only identities and synthetic fixtures. Never use a
user account, public guild, real meeting, or customer audio.
