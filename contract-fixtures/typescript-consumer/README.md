# Exact TypeScript VoiceText contract fixture

This dependency-free TypeScript fixture is the release-gate consumer for the public VoiceText
batch-v2, batch-v3, and live-v2 contracts. It is valid TypeScript and uses only Node.js built-ins so
the gate cannot drift with a mutable consumer repository or package registry resolution.

`SHA256SUMS` pins the exact reviewed fixture bytes. `scripts/run-production-composition-gate.sh`
verifies that digest before running it against the release-mode gateway binary, disposable
PostgreSQL, and loopback-only provider wire fakes. Update the fixture and checksum together only
after an explicit contract review.
