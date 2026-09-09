# Dependency rules

```text
domain <- application <- adapters <- composition
```

## Domain

Domain modules use the Rust standard library only. They own deterministic value objects and state
transitions. They cannot read configuration or know provider, transport, persistence, or runtime
types.

## Application

Application modules import domain modules and define small consumer-owned ports. Batch recognition,
live recognition, job persistence, audio spooling, and observability are separate capabilities.

## Adapters

Deepgram and ElevenLabs adapters translate provider wire data to application models. HTTP,
WebSocket, PostgreSQL, file storage, and metrics adapters remain replaceable. Provider DTOs are
private to their adapter.

## Composition

Composition reads secret files and configuration, creates concrete adapters, and selects enabled
profiles. No request can choose an unconfigured profile. There is no implicit provider fallback.

`cargo xtask verify` classifies every production Rust file fail-closed and rejects forbidden
directional imports or oversized files.
