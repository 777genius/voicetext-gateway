#!/bin/sh
set -eu

workflow=.github/workflows/ci.yml

line_of() {
  grep -nF "$1" "$workflow" | head -n 1 | cut -d: -f1
}

quarantine=$(line_of "Build exact remote source into the job-local quarantine registry")
scan=$(line_of "Apply fail-closed vulnerability policy")
acceptance=$(line_of "Consume protected reviewer and provider-canary evidence")
stage=$(line_of "Stage the verified digest under a deterministic quarantine identity")
attest=$(line_of "Cryptographically attest source-to-image build provenance")
verify=$(line_of "Verify registry attestations against the exact repository identity")
assets=$(line_of "Publish evidence as immutable release-lifetime assets")
publish=$(line_of "Publish final public image tags as the last irreversible step")

test "$quarantine" -lt "$scan"
test "$scan" -lt "$acceptance"
test "$acceptance" -lt "$stage"
test "$stage" -lt "$attest"
test "$attest" -lt "$verify"
test "$verify" -lt "$assets"
test "$assets" -lt "$publish"
test -z "$(tail -n +$((publish + 1)) "$workflow" | grep -E '^[[:space:]]+- name:' || true)"
grep -F 'environment: release-publication' "$workflow" >/dev/null
grep -F 'quarantine_tag="quarantine-${SOURCE_SHA}"' "$workflow" >/dev/null
grep -F 'scripts/verify-release-acceptance.sh "$SOURCE_SHA"' "$workflow" >/dev/null
grep -F 'durable_startup_recovery_is_exactly_once -- --ignored --exact' \
  scripts/run-production-composition-gate.sh >/dev/null
grep -F 'ref=${SOURCE_REF}&checksum=${SOURCE_SHA}' "$workflow" >/dev/null
grep -F -- '--provenance=mode=max' "$workflow" >/dev/null
grep -F 'docker pull "$exact_image"' "$workflow" >/dev/null
grep -F 'syft "docker:${image}@${digest}"' "$workflow" >/dev/null
grep -F 'docker buildx imagetools create --tag "${image}:${SOURCE_SHA}" --tag "${image}:${RELEASE_TAG}" "$source"' "$workflow" >/dev/null
grep -F 'test "$(crane digest "${image}:${RELEASE_TAG}")"' "$workflow" >/dev/null

echo "release workflow protected publication ordering passed"
