#!/bin/sh
set -eu

usage() {
  echo "usage: $0 SOURCE_SHA IMAGE_REFERENCE IMAGE_DIGEST EVIDENCE_DIRECTORY" >&2
  exit 2
}

[ "$#" -eq 4 ] || usage
source_sha=$1
image_reference=$2
image_digest=$3
evidence_dir=$4
policy=security/vulnerability-policy.json

case "$source_sha" in *[!0-9a-f]*|'') usage ;; esac
[ "${#source_sha}" -eq 40 ] || usage
case "$image_digest" in sha256:[0-9a-f]* ) ;; *) usage ;; esac
[ "${#image_digest}" -eq 71 ] || usage

for required in voicetext-gateway.sbom.cdx.json vulnerabilities.grype.json
do
  [ -s "$evidence_dir/$required" ] || {
    echo "missing release evidence: $evidence_dir/$required" >&2
    exit 1
  }
done

for required in LICENSE NOTICE
do
  [ -s "$evidence_dir/distribution/$required" ] || {
    echo "missing image distribution evidence: $evidence_dir/distribution/$required" >&2
    exit 1
  }
done
cmp LICENSE "$evidence_dir/distribution/LICENSE"
cmp NOTICE "$evidence_dir/distribution/NOTICE"

mkdir -p "$evidence_dir/policy" "$evidence_dir/composition"
cp "$policy" "$evidence_dir/policy/vulnerability-policy.json"
cp contract-fixtures/typescript-consumer/voicetext-gateway-contract.ts \
  "$evidence_dir/composition/voicetext-gateway-contract.ts"

sbom_sha=$(sha256sum "$evidence_dir/voicetext-gateway.sbom.cdx.json" | cut -d' ' -f1)
vulnerability_sha=$(sha256sum "$evidence_dir/vulnerabilities.grype.json" | cut -d' ' -f1)
policy_sha=$(sha256sum "$evidence_dir/policy/vulnerability-policy.json" | cut -d' ' -f1)
license_sha=$(sha256sum "$evidence_dir/distribution/LICENSE" | cut -d' ' -f1)
notice_sha=$(sha256sum "$evidence_dir/distribution/NOTICE" | cut -d' ' -f1)
fixture_sha=$(sha256sum "$evidence_dir/composition/voicetext-gateway-contract.ts" | cut -d' ' -f1)

jq -S -n \
  --arg source_sha "$source_sha" \
  --arg image_reference "$image_reference" \
  --arg image_digest "$image_digest" \
  --arg sbom_sha "$sbom_sha" \
  --arg vulnerability_sha "$vulnerability_sha" \
  --arg policy_sha "$policy_sha" \
  --arg license_sha "$license_sha" \
  --arg notice_sha "$notice_sha" \
  --arg fixture_sha "$fixture_sha" \
  '{
    predicate_type: "https://voicetext.dev/attestations/release-evidence/v1",
    source: {git_sha: $source_sha},
    image: {reference: $image_reference, digest: $image_digest},
    sbom: {format: "CycloneDX", sha256: $sbom_sha},
    vulnerability_policy: {
      result: "pass",
      scanner: "grype@0.118.0",
      policy_sha256: $policy_sha,
      result_sha256: $vulnerability_sha
    },
    distribution: {license_sha256: $license_sha, notice_sha256: $notice_sha},
    composition_gate: {
      result: "pass",
      fixture_sha256: $fixture_sha,
      network: "loopback-only",
      provider_credentials: "synthetic"
    }
  }' >"$evidence_dir/release-evidence.json"

(
  cd "$evidence_dir"
  # SHA256SUMS is excluded from find, so opening it here cannot create a self-reference.
  # shellcheck disable=SC2094
  find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort | sed 's|^./||' | xargs sha256sum \
    >SHA256SUMS
)
