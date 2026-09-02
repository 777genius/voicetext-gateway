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
  def evidence($provider; $model; $mode; $version; $id):
    {provider: $provider, model: $model, mode: $mode, contract_version: $version,
     language: "multi",
     fixture_digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111",
     result_digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222",
     request_id: ("schema-request-" + $id), result_id: ("schema-result-" + $id),
     started_at: "2026-09-02T10:00:00Z", completed_at: "2026-09-02T10:00:01Z",
     latency_ms: 1000, error_classification: "none",
     credential_owner: "test-credential-owner"};
  def batch($provider; $model; $version; $id):
    evidence($provider; $model; "batch"; $version; $id);
  def live($provider; $model; $id):
    evidence($provider; $model; "live"; 2; $id) +
      {ack_count: 1, finalize_status: "observed"};
  {source_sha: $source_sha, image_digest: $image_digest, result: "pass",
   campaign_id: "verifier-schema-test", campaign_owner: "test-campaign-owner",
   campaign_approval: "test-approval-1", completed_at: "2026-09-02T11:00:00Z",
   checks: [
     {profile: "deepgram-batch", result: "pass",
      batch: batch("deepgram"; "nova-3"; 2; "dg-batch"), live: null},
     {profile: "deepgram-live", result: "pass", batch: null,
      live: live("deepgram"; "nova-3"; "dg-live")},
     {profile: "deepgram-batch-elevenlabs-live", result: "pass",
      batch: batch("deepgram"; "nova-3"; 2; "mixed-dg"),
      live: live("elevenlabs"; "scribe_v2_realtime"; "mixed-el-live")},
     {profile: "elevenlabs-batch", result: "pass",
      batch: batch("elevenlabs"; "scribe_v2"; 3; "el-batch"), live: null},
     {profile: "elevenlabs-live", result: "pass", batch: null,
      live: live("elevenlabs"; "scribe_v2_realtime"; "el-live")},
     {profile: "elevenlabs-batch-deepgram-live", result: "pass",
      batch: batch("elevenlabs"; "scribe_v2"; 3; "mixed-el"),
      live: live("deepgram"; "nova-3"; "mixed-dg-live")}
   ]}' >"$evidence/acceptance/provider-canary.json"

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

reject_canary_mutation() {
  description=$1
  filter=$2
  cp "$evidence/acceptance/provider-canary.json" "$sandbox/canary.json"
  jq "$filter" "$sandbox/canary.json" >"$evidence/acceptance/provider-canary.json"
  if scripts/verify-release-acceptance.sh \
    "$source_sha" "$image_digest" "$evidence" >/dev/null 2>&1; then
    echo "$description unexpectedly passed" >&2
    exit 1
  fi
  mv "$sandbox/canary.json" "$evidence/acceptance/provider-canary.json"
}

reject_canary_mutation "unknown canary property" '.unexpected = true'
reject_canary_mutation "missing fixture digest" 'del(.checks[0].batch.fixture_digest)'
reject_canary_mutation "mismatched provider model" '.checks[1].live.model = "wrong-model"'
reject_canary_mutation "zero live ACK count" '.checks[1].live.ack_count = 0'
reject_canary_mutation "unobserved live finalize" \
  '.checks[1].live.finalize_status = "requested"'
reject_canary_mutation "missing campaign ownership" 'del(.campaign_owner)'

cp "$evidence/acceptance/reviewer-approval.json" "$sandbox/reviewer.json"
jq '.unexpected = true' "$sandbox/reviewer.json" \
  >"$evidence/acceptance/reviewer-approval.json"
if scripts/verify-release-acceptance.sh "$source_sha" "$image_digest" "$evidence" >/dev/null 2>&1; then
  echo "unknown reviewer property unexpectedly passed" >&2
  exit 1
fi
mv "$sandbox/reviewer.json" "$evidence/acceptance/reviewer-approval.json"

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
