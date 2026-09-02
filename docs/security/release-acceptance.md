# Release, license, and private-adoption acceptance

This checklist is fail-closed. A source tag, crate, container, or private-SaaS adoption is not
approved merely because deterministic CI is green. Record each artifact digest and reviewer in the
release evidence bundle; never store credentials, provider response bodies, customer audio, or
private repository content in that bundle.

## Apache-2.0 publication checklist

- [ ] The repository owner confirms the right to publish every selected implementation and the
  provenance inventory at the release commit.
- [ ] `LICENSE` is the unmodified Apache License 2.0 text; package metadata declares
  `Apache-2.0`; source and binary distributions include the license.
- [ ] A legal reviewer confirms whether a `NOTICE` file or third-party attribution bundle is
  required. If required, it is present in source, crate, and container distributions.
- [ ] `cargo deny check` passes against the locked dependency graph, and exceptions have an owner,
  rationale, and expiry.
- [ ] Tracked-tree and complete-history secret scans pass at the exact release commit; a manual
  extraction-inventory diff finds no private endpoints, customer data, SaaS auth/billing code, or
  private Git history.
- [ ] The exact digest-pinned image is built from a Git `ref` plus matching `checksum`; its SBOM is
  generated, parsed, retained, and linked to the image digest. Vulnerability results are reviewed
  under the release policy.
- [ ] Required `exact-head` and `production-composition` jobs pass for the tagged SHA. The latter
  runs the release-mode binary, disposable PostgreSQL, loopback provider fakes, and the
  SHA-256-pinned independent TypeScript fixture without provider keys or mutable consumer input,
  then executes the exact ignored durable startup-recovery test against disposable PostgreSQL.
- [ ] The non-root/read-only/no-new-privileges runtime assertions and mounted-file secret-custody
  checks pass. A rollback image digest and compatible database/spool backup are recorded.
- [ ] Each real-provider and private-guild gate uses only test identities and synthetic fixtures and
  has explicit human approval. Normal CI never performs these external effects.

## Provider qualification evidence mapping

Deterministic evidence proves implementation and conformance, not acoustic quality or provider
availability. The provider capability matrix is the claim surface. For every row and every language
claim, retain one bounded canary record containing the exact release SHA and image digest, provider
and model, mode and contract version, language selection, synthetic-fixture digest, request/result
identifiers, start/end timestamps, result digest, error classification, latency, ACK counts for
live, and finalization status. It must also identify the test-only credential owner and campaign
approval without including the credential.

| Claim | Checked-in conformance evidence | Additional release evidence required |
| --- | --- | --- |
| Deepgram Nova-3 batch v2 | Provider wire tests and `exact_batch_v2_and_v3_wire_contracts` | One planned batch effect for each claimed language/selection and fixture |
| ElevenLabs Scribe v2 batch v3 | Provider wire tests and `exact_batch_v2_and_v3_wire_contracts` | One planned batch effect for each claimed language/selection and fixture |
| Deepgram Nova-3 live v2 | Provider wire tests and `exact_live_v2_ready_ack_final_and_finalize_contract` | One session proving ACK/finalize evidence for each claimed language/selection |
| ElevenLabs Scribe v2 Realtime live v2 | Provider wire tests and `exact_live_v2_ready_ack_final_and_finalize_contract` | One session proving ACK/commit evidence for each claimed language/selection |
| Mixed provider selection | Production-composition fake-provider gate | One Deepgram-batch/ElevenLabs-live and one ElevenLabs-batch/Deepgram-live campaign |
| Discord compatibility | Checked-in TypeScript consumer gate | Approved official bot, private test guild, synthetic audio, and disposable storage campaign |

An absent, partial, mismatched, or unreviewed record leaves the associated provider/language claim
unqualified. Provider documentation and successful fake-provider tests cannot fill that gap.

## Deferred private VoiceText SaaS adoption

Private adoption starts only after a tagged public release satisfies the publication checklist and
all of the following evidence exists:

- the private consumer pins immutable crate and image identities and passes the independent
  VoiceText batch-v2, batch-v3, and live-v2 black-box suite;
- a dependency-boundary review proves that public users, OAuth, licensing, billing, quota, Redis,
  TTS, LLM, and private transport DTOs did not enter this STT repository;
- shadow or test-only traffic compares normalized results without duplicating a paid effect, and
  unknown outcomes remain fenced rather than retried or switched to another provider;
- migrations are forward/backward compatibility-reviewed, PostgreSQL and spool are backed up as one
  unit, and in-flight jobs can drain on the old deployment;
- operators have dashboards for readiness, saturation, provider failure classes, finalize timeout,
  and outcome-unknown states, plus an approved change window and accountable rollback owner.

Rollback requires the previous known-good image and crate digests, their exact configuration and
secret-file mounts, a compatible database/spool backup or proven backward-compatible schema, and a
rehearsed decision threshold. Stop new admission, let known work drain, preserve unknown-outcome
evidence, restore the previous digest, prove readiness and contract conformance, then resume. Never
roll back by rebinding an accepted job/session, automatically falling back providers, resubmitting
an uncertain paid request, or deleting authoritative spool evidence.

Pipecat remains future-only documentation. It is not part of release qualification, private
adoption, runtime composition, or rollback until a separately approved conformance-tested adapter
exists.

## Immutable release evidence and retention

For a `v*` tag, `release-evidence` runs only after both required jobs succeed and enters the
protected `release-publication` environment. Configure both that environment and the separate
`canary-approval` environment with required reviewers who cannot be bypassed by a committer. The
operator first creates a draft GitHub Release for the exact tag and attaches the retained, real
canary's canonical `provider-canary.json`, `campaign-manifest.json`, and `fixture-manifest.json`.
These records come from the test-only runner identity pinned in `security/release-trust-policy.json`
and carry its exact revision; they are not hand-authored PASS claims. Retain runner logs, the
synthetic fixture, and bounded provider receipts for audit (never credentials, customer audio, or
provider response bodies).

Dispatch `.github/workflows/canary-approval.yml` at the candidate source SHA. Its protected
`canary-approval` review verifies the candidate tag/image, trusted runner identity, strict canonical
records, manifest bindings, and all eight fresh effects. It emits `reviewer-approval.json` whose
SHA-256 bindings cover the canary payload, both manifests, trust policy, exact source SHA, image
digest, campaign ID, and runner identity/revision. GitHub OIDC signs that approval as a
build-provenance attestation, which is attached as `reviewer-approval.sigstore.json` without
overwrite. No private signing key or caller-selected trust root is accepted.

The publication workflow downloads all five records and verifies the attestation against the
repository, exact candidate workflow revision, checked-in signer workflow, and GitHub-hosted runner
before staging or publishing a public tag. A committer may write canary-shaped JSON, but cannot
produce the OIDC attestation from the protected job without its independent environment review.
Missing, concatenated, non-canonical, duplicate-key, unknown-key, partial, reused-effect, or
digest-mismatched records fail closed. Synthetic fixtures in `scripts/test-release-evidence.sh`
prove parser/policy behavior only and are never real-provider qualification evidence.

`scripts/verify-release-acceptance.sh` defines the exact six-profile/eight-effect policy. Provider
request, result, and effect IDs are globally unique, so mixed checks cannot reuse standalone halves.
Every effect binds outcome and timestamps. Batch terminal status is tied to its provider result; live
evidence ties the complete ACK range and accepted-frame digest to a flushed finalize and the same
provider result. Each bounded JSON record is at most 64 KiB and contains exactly one top-level value;
the Sigstore bundle is bounded separately at 1 MiB.

The job rebuilds the image from the exact remote Git ref plus checksum and first assigns only the
deterministic `quarantine-<source-sha>` GHCR identity. GitHub artifact attestations
cryptographically bind that digest to SLSA build provenance, the digest-bearing CycloneDX SBOM, and
the deterministic release predicate. The predicate hashes the reviewer and canary records as well
as the exact source SHA, SBOM, Grype result and fail-closed policy, Apache-2.0 license, NOTICE, and
TypeScript composition fixture. CI verifies every attestation against the repository identity and
uploads immutable evidence assets before creating the final source-SHA and version tags in one last
publication step.

GitHub Actions artifacts are a convenience copy retained for 90 days, the maximum generally
available to public repositories; repository policy can further reduce that period. They are not
the release-lifetime record. The digest-named archive, predicate, SBOM, vulnerability result,
applied policy, protected acceptance records, exact image LICENSE/NOTICE, and pinned composition
fixture are uploaded without overwrite to the draft GitHub Release for the exact tag. GitHub
Release assets and GHCR attestations follow the release/package lifetime and remain until an
authorized maintainer deletes them. Configure branch protection/rulesets to require the workflow
jobs named `exact-head` and `production-composition`; only the protected `release-evidence` job is
authorized to stage or publish images and release evidence.
