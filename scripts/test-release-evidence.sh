#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

sandbox=$(mktemp -d "${TMPDIR:-/tmp}/voicetext-release-evidence-test.XXXXXX")
cleanup() {
  find "$sandbox" -depth -delete
}
trap cleanup EXIT INT TERM

evidence="$sandbox/evidence"
source_sha=06d80568dd0ffc5d89f6b21b0514ee6824ab1d22
image_digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
mkdir -p "$evidence/distribution"
cp LICENSE NOTICE "$evidence/distribution/"
mkdir -p "$evidence/acceptance"
jq -n --arg source_sha "$source_sha" --arg image_digest "$image_digest" '
  {source_sha: $source_sha, image_digest: $image_digest, decision: "approved",
   reviewer_login: "release-reviewer", approved_at: "2026-09-02T12:00:00Z"}' \
  >"$evidence/acceptance/reviewer-approval.json"
jq -n --arg source_sha "$source_sha" --arg image_digest "$image_digest" '
  {source_sha: $source_sha, image_digest: $image_digest, result: "pass",
   campaign_id: "synthetic-canary-1", completed_at: "2026-09-02T11:00:00Z", checks:
   ["deepgram-batch", "deepgram-live", "deepgram-batch-elevenlabs-live",
    "elevenlabs-batch", "elevenlabs-live", "elevenlabs-batch-deepgram-live"]
   | map({profile: ., result: "pass"})}' >"$evidence/acceptance/provider-canary.json"

jq -n --arg source_sha "$source_sha" --arg image_digest "$image_digest" '
  {
    bomFormat: "CycloneDX",
    specVersion: "1.6",
    components: [{type: "application", name: "voicetext-gateway", version: "test"}],
    metadata: {properties: [
      {name: "org.opencontainers.image.revision", value: $source_sha},
      {name: "org.opencontainers.image.digest", value: $image_digest}
    ]}
  }
' >"$evidence/voicetext-gateway.sbom.cdx.json"
jq -n '
  {descriptor: {name: "grype", version: "0.118.0"}, matches: [], ignoredMatches: []}
' >"$evidence/vulnerabilities.grype.json"

scripts/create-release-evidence.sh "$source_sha" \
  ghcr.io/777genius/voicetext-gateway "$image_digest" "$evidence"
scripts/verify-release-evidence.sh "$evidence"

cp "$evidence/acceptance/provider-canary.json" "$sandbox/canary.json"
jq '.checks = .checks[:-1]' "$sandbox/canary.json" >"$evidence/acceptance/provider-canary.json"
if scripts/verify-release-acceptance.sh "$source_sha" "$image_digest" "$evidence" >/dev/null 2>&1; then
  echo "incomplete provider-canary evidence unexpectedly passed" >&2
  exit 1
fi
mv "$sandbox/canary.json" "$evidence/acceptance/provider-canary.json"

cp "$evidence/acceptance/provider-canary.json" "$sandbox/canary.json"
jq '.checks[0].result = "fail"' "$sandbox/canary.json" \
  >"$evidence/acceptance/provider-canary.json"
if scripts/verify-release-acceptance.sh "$source_sha" "$image_digest" "$evidence" >/dev/null 2>&1; then
  echo "failed provider-canary check unexpectedly passed" >&2
  exit 1
fi
mv "$sandbox/canary.json" "$evidence/acceptance/provider-canary.json"

cp "$evidence/acceptance/reviewer-approval.json" "$sandbox/reviewer.json"
jq '.padding = ("x" * 65536)' "$sandbox/reviewer.json" \
  >"$evidence/acceptance/reviewer-approval.json"
if scripts/verify-release-acceptance.sh "$source_sha" "$image_digest" "$evidence" >/dev/null 2>&1; then
  echo "oversized reviewer evidence unexpectedly passed" >&2
  exit 1
fi
mv "$sandbox/reviewer.json" "$evidence/acceptance/reviewer-approval.json"

jq '.matches = [{vulnerability: {severity: "High"}}]' \
  "$evidence/vulnerabilities.grype.json" >"$sandbox/tampered.json"
mv "$sandbox/tampered.json" "$evidence/vulnerabilities.grype.json"
if scripts/verify-release-evidence.sh "$evidence" >/dev/null 2>&1; then
  echo "high-severity vulnerability evidence unexpectedly passed" >&2
  exit 1
fi
