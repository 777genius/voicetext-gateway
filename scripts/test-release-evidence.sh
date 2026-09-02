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

jq '.matches = [{vulnerability: {severity: "High"}}]' \
  "$evidence/vulnerabilities.grype.json" >"$sandbox/tampered.json"
mv "$sandbox/tampered.json" "$evidence/vulnerabilities.grype.json"
if scripts/verify-release-evidence.sh "$evidence" >/dev/null 2>&1; then
  echo "high-severity vulnerability evidence unexpectedly passed" >&2
  exit 1
fi
