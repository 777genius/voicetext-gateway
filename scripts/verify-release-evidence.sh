#!/bin/sh
set -eu

[ "$#" -eq 1 ] || {
  echo "usage: $0 EVIDENCE_DIRECTORY" >&2
  exit 2
}
evidence_dir=$1
predicate="$evidence_dir/release-evidence.json"

[ "$(sha256sum LICENSE | cut -d' ' -f1)" = \
  c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 ]
jq -e '
  .scanner.name == "grype" and
  .scanner.version == "0.118.0" and
  .fail_on_severities == ["Critical", "High"] and
  .ignored_vulnerabilities == []
' "$evidence_dir/policy/vulnerability-policy.json" >/dev/null

cmp LICENSE "$evidence_dir/distribution/LICENSE"
cmp NOTICE "$evidence_dir/distribution/NOTICE"
cmp security/vulnerability-policy.json "$evidence_dir/policy/vulnerability-policy.json"
cmp contract-fixtures/typescript-consumer/voicetext-gateway-contract.ts \
  "$evidence_dir/composition/voicetext-gateway-contract.ts"

(cd "$evidence_dir" && sha256sum --check SHA256SUMS)
jq -e '
  .predicate_type == "https://voicetext.dev/attestations/release-evidence/v1" and
  (.source.git_sha | test("^[0-9a-f]{40}$")) and
  (.image.digest | test("^sha256:[0-9a-f]{64}$")) and
  .sbom.format == "CycloneDX" and
  .vulnerability_policy.result == "pass" and
  (.protected_acceptance.reviewer_sha256 | test("^[0-9a-f]{64}$")) and
  (.protected_acceptance.canary_sha256 | test("^[0-9a-f]{64}$")) and
  .composition_gate.result == "pass"
' "$predicate" >/dev/null

source_sha=$(jq -r .source.git_sha "$predicate")
image_digest=$(jq -r .image.digest "$predicate")
scripts/verify-release-acceptance.sh "$source_sha" "$image_digest" "$evidence_dir"
jq -e --arg source_sha "$source_sha" --arg image_digest "$image_digest" '
  .bomFormat == "CycloneDX" and
  (.components | type == "array" and length > 0) and
  (.metadata.properties | any(.name == "org.opencontainers.image.revision" and .value == $source_sha)) and
  (.metadata.properties | any(.name == "org.opencontainers.image.digest" and .value == $image_digest))
' "$evidence_dir/voicetext-gateway.sbom.cdx.json" >/dev/null

jq -e '
  .descriptor.name == "grype" and
  .descriptor.version == "0.118.0" and
  ([.ignoredMatches[]?] | length == 0) and
  ([.matches[]? | .vulnerability.severity]
    | map(select(. == "Critical" or . == "High")) | length == 0)
' "$evidence_dir/vulnerabilities.grype.json" >/dev/null

[ "$(jq -r .sbom.sha256 "$predicate")" = \
  "$(sha256sum "$evidence_dir/voicetext-gateway.sbom.cdx.json" | cut -d' ' -f1)" ]
[ "$(jq -r .vulnerability_policy.result_sha256 "$predicate")" = \
  "$(sha256sum "$evidence_dir/vulnerabilities.grype.json" | cut -d' ' -f1)" ]
[ "$(jq -r .vulnerability_policy.policy_sha256 "$predicate")" = \
  "$(sha256sum "$evidence_dir/policy/vulnerability-policy.json" | cut -d' ' -f1)" ]
[ "$(jq -r .distribution.license_sha256 "$predicate")" = \
  "$(sha256sum "$evidence_dir/distribution/LICENSE" | cut -d' ' -f1)" ]
[ "$(jq -r .distribution.notice_sha256 "$predicate")" = \
  "$(sha256sum "$evidence_dir/distribution/NOTICE" | cut -d' ' -f1)" ]
[ "$(jq -r .composition_gate.fixture_sha256 "$predicate")" = \
  "$(sha256sum "$evidence_dir/composition/voicetext-gateway-contract.ts" | cut -d' ' -f1)" ]
[ "$(jq -r .protected_acceptance.reviewer_sha256 "$predicate")" = \
  "$(sha256sum "$evidence_dir/acceptance/reviewer-approval.json" | cut -d' ' -f1)" ]
[ "$(jq -r .protected_acceptance.canary_sha256 "$predicate")" = \
  "$(sha256sum "$evidence_dir/acceptance/provider-canary.json" | cut -d' ' -f1)" ]
