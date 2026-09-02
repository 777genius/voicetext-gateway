#!/bin/sh
set -eu

workflow=.github/workflows/ci.yml

line_of() {
  grep -nF "$1" "$workflow" | head -n 1 | cut -d: -f1
}

quarantine=$(line_of "Build exact remote source into the job-local quarantine registry")
scan=$(line_of "Apply fail-closed vulnerability policy")
evidence=$(line_of "Create and verify deterministic release predicate")
publish=$(line_of "Publish only the exact image that passed every quarantine gate")

test "$quarantine" -lt "$scan"
test "$scan" -lt "$evidence"
test "$evidence" -lt "$publish"
grep -F 'ref=${SOURCE_REF}&checksum=${SOURCE_SHA}' "$workflow" >/dev/null
grep -F -- '--provenance=mode=max' "$workflow" >/dev/null
grep -F 'docker pull "$exact_image"' "$workflow" >/dev/null
grep -F 'syft "docker:${image}@${digest}"' "$workflow" >/dev/null
grep -F 'docker buildx imagetools create --tag "${image}:${SOURCE_SHA}" "$source"' "$workflow" >/dev/null
grep -F 'docker buildx imagetools create --tag "${image}:${RELEASE_TAG}" "$source"' "$workflow" >/dev/null
grep -F 'test "$(crane digest "${image}:${RELEASE_TAG}")"' "$workflow" >/dev/null

echo "release workflow quarantine ordering passed"
