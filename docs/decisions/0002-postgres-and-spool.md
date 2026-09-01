# ADR-0002: PostgreSQL metadata ledger and durable audio spool

Status: accepted for V1

## Decision

Persist batch identity, fingerprint, profile, submission fence, provider reference, terminal state,
and bounded result in PostgreSQL. Persist accepted audio in an atomic filesystem spool on a durable
volume so an accepted pre-egress job can resume after restart.

The storage contracts remain application-owned ports. A future SQLite adapter can be added without
changing domain or provider adapters.

## Consequences

The Discord deployment reuses its existing PostgreSQL service. A standalone Compose deployment
starts PostgreSQL beside the gateway. Gateway V1 remains single-replica for live sessions; batch
persistence does not imply live takeover support.
