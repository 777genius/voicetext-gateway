#!/bin/sh
set -eu

[ "$#" -eq 3 ] || {
  echo "usage: $0 SOURCE_SHA IMAGE_DIGEST EVIDENCE_DIRECTORY" >&2
  exit 2
}
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
source_sha=$1
image_digest=$2
evidence_dir=$3
acceptance="$evidence_dir/acceptance"
approval="$acceptance/reviewer-approval.json"
bundle="$acceptance/reviewer-approval.sigstore.json"
canary="$acceptance/provider-canary.json"
campaign="$acceptance/campaign-manifest.json"
fixtures="$acceptance/fixture-manifest.json"
policy=security/release-trust-policy.json

for required in "$approval" "$bundle" "$canary" "$campaign" "$fixtures" "$policy"; do
  [ -s "$required" ] || {
    echo "missing protected acceptance evidence: $required" >&2
    exit 1
  }
done
for bounded in "$approval" "$canary" "$campaign" "$fixtures"; do
  [ "$(wc -c <"$bounded")" -le 65536 ] || {
    echo "oversized protected acceptance evidence: $bounded" >&2
    exit 1
  }
done
[ "$(wc -c <"$bundle")" -le 1048576 ] || {
  echo "oversized approval attestation bundle: $bundle" >&2
  exit 1
}

scripts/verify_json_record.py "$approval" "$bundle" "$canary" "$campaign" "$fixtures" "$policy"
scripts/verify_release_acceptance.py "$source_sha" "$image_digest" "$evidence_dir"

repository=$(jq -er '.approval_attestation.repository' "$policy")
signer_workflow=$(jq -er '.approval_attestation.signer_workflow' "$policy")
predicate_type=$(jq -er '.approval_attestation.predicate_type' "$policy")
protected_environment=$(jq -er '.approval_attestation.protected_environment' "$policy")
signer_digest=$(jq -er '.approval_workflow_revision' "$approval")
[ "$(jq -er '.protected_environment' "$approval")" = "$protected_environment" ]

command -v gh >/dev/null 2>&1 || {
  echo "GitHub CLI is required to authenticate reviewer approval" >&2
  exit 1
}
gh attestation verify "$approval" \
  --bundle "$bundle" \
  --repo "$repository" \
  --signer-workflow "$signer_workflow" \
  --signer-digest "$signer_digest" \
  --source-digest "$signer_digest" \
  --predicate-type "$predicate_type" \
  --deny-self-hosted-runners >/dev/null
