# End-to-end implementation plan

1. Freeze the STT-only scope, VoiceText compatibility matrix, OSS provenance, license, and deferred
   Pipecat decision.
2. Establish an isolated Rust workspace with fail-closed architecture, source-size, formatting,
   lint, test, documentation, dependency-license, and vulnerability gates.
3. Implement deterministic batch and live state machines and narrow application ports.
4. Selectively extract and refactor the proven Deepgram and ElevenLabs provider mechanics from the
   private Rust backend. Do not copy SaaS orchestration, auth, billing, Redis, or generic shared code.
5. Implement exact batch v2, batch v3, and live v2 inbound contracts, machine bearer auth,
   PostgreSQL job persistence, a durable bounded audio spool, health, metrics, and graceful
   shutdown.
6. Run providerless black-box TypeScript-client-to-Rust-gateway conformance, failure, idempotency,
   restart, Opus, finalize, resource-bound, and security tests.
7. Add standalone and Discord Meeting Assistant Docker Compose deployment with non-root containers
   and secret files.
8. Only after the evidence-backed adoption prerequisites and rollback rehearsal in
   `docs/security/release-acceptance.md`, pin tagged public crates and image digests in the private
   VoiceText SaaS and remove duplicated provider mechanics without publishing SaaS features.
9. Run exactly one bounded test-only canary for each provider/mode, both mixed-profile combinations,
   and the isolated private-guild Discord campaign.
10. Publish the approved license, crates, image, SBOM, compatibility documentation, migration and
    rollback runbooks, language-capability wording, and future Pipecat adapter guide.
