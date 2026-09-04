#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
sandbox=$(mktemp -d "${TMPDIR:-/tmp}/voicetext-release-evidence-test.XXXXXX")
cleanup() { find "$sandbox" -depth -delete; }
trap cleanup EXIT INT TERM

evidence="$sandbox/evidence"
acceptance="$evidence/acceptance"
source_sha=06d80568dd0ffc5d89f6b21b0514ee6824ab1d22
image_digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
runner_revision=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
mkdir -p "$evidence/distribution" "$acceptance" "$sandbox/bin"
cp LICENSE NOTICE "$evidence/distribution/"

# These are synthetic verifier fixtures. They prove parser and policy behavior only and are not
# real-provider qualification evidence.
jq -S -c -n '{schema_version:1,campaign_id:"synthetic-verifier-campaign",fixtures:[{fixture_id:"synthetic-audio",sha256:"sha256:1111111111111111111111111111111111111111111111111111111111111111"}]}' >"$acceptance/fixture-manifest.json"
jq -S -c -n --arg source "$source_sha" --arg image "$image_digest" --arg revision "$runner_revision" '
  def bound($position;$profile;$kind;$number;$operation_kind):
    {position:$position,profile:$profile,kind:$kind,fixture_id:"synthetic-audio",
     fixture_digest:"sha256:1111111111111111111111111111111111111111111111111111111111111111",
     result_digest:("sha256:" + (($number|tostring) * 64)[0:64]),
     effect_id:(if $number == "1" then "native-operation-6" else ("verifier-effect-"+$number) end),
     provider_operation:{kind:$operation_kind,
       id:(if $number == "5" then "native-operation-1" else ("native-operation-"+$number) end)}};
  {schema_version:2,source_sha:$source,image_digest:$image,campaign_id:"synthetic-verifier-campaign",
   runner:{identity:"github.com/777genius/voicetext-canary/.github/workflows/run.yml",revision:$revision},
   effects:[
    bound("standalone-dg-batch";"deepgram-batch";"batch";"1";"deepgram_request_id"),
    bound("standalone-dg-live";"deepgram-live";"live";"2";"deepgram_request_id"),
    bound("mixed-dg-batch";"deepgram-batch-elevenlabs-live";"batch";"3";"deepgram_request_id"),
    bound("mixed-el-live";"deepgram-batch-elevenlabs-live";"live";"4";"elevenlabs_session_id"),
    bound("standalone-el-batch";"elevenlabs-batch";"batch";"5";"elevenlabs_transcription_id"),
    bound("standalone-el-live";"elevenlabs-live";"live";"6";"elevenlabs_http_request_id"),
    bound("mixed-el-batch";"elevenlabs-batch-deepgram-live";"batch";"7";"elevenlabs_http_request_id"),
    bound("mixed-dg-live";"elevenlabs-batch-deepgram-live";"live";"8";"deepgram_request_id")
  ]}' >"$acceptance/campaign-manifest.json"
campaign_sha=$(sha256sum "$acceptance/campaign-manifest.json" | cut -d' ' -f1)
fixture_sha=$(sha256sum "$acceptance/fixture-manifest.json" | cut -d' ' -f1)
jq -S -c -n --arg source "$source_sha" --arg image "$image_digest" --arg revision "$runner_revision" \
  --arg campaign_sha "$campaign_sha" --arg fixture_sha "$fixture_sha" \
  --slurpfile manifest "$acceptance/campaign-manifest.json" '
  def found($position): $manifest[0].effects[] | select(.position==$position);
  def base($position;$provider;$model;$mode;$version):
    found($position) + {provider:$provider,model:$model,mode:$mode,contract_version:$version,
      language:"multi",started_at:"2026-09-02T10:00:00.950Z",completed_at:"2026-09-02T10:00:01.073Z",
      latency_ms:123,outcome:"pass",error_classification:"none"}
      | del(.profile,.kind);
  def batch($position;$provider;$model;$version):
    base($position;$provider;$model;"batch";$version) +
      {provider_terminal:{status:"completed",provider_operation:(found($position).provider_operation),
        effect_id:(found($position).effect_id),result_digest:(found($position).result_digest),
        observed_at:"2026-09-02T10:00:01.073Z"}};
  def live($position;$provider;$model):
    base($position;$provider;$model;"live";2) + {accepted_frame_count:2,
      accepted_frames_digest:"sha256:9999999999999999999999999999999999999999999999999999999999999999",
      ack_first:1,ack_last:2,finalize:{status:"flushed",provider_operation:(found($position).provider_operation),
        effect_id:(found($position).effect_id),result_digest:(found($position).result_digest),
        terminal_at:"2026-09-02T10:00:01.073Z"}};
  {schema_version:2,source_sha:$source,image_digest:$image,campaign_id:"synthetic-verifier-campaign",
   runner:{identity:"github.com/777genius/voicetext-canary/.github/workflows/run.yml",revision:$revision},
   credential_owner:"synthetic-verifier-owner",campaign_manifest_sha256:$campaign_sha,
   fixture_manifest_sha256:$fixture_sha,result:"pass",completed_at:"2026-09-02T11:00:00.000Z",
   checks:[
    {profile:"deepgram-batch",result:"pass",batch:batch("standalone-dg-batch";"deepgram";"nova-3";2),live:null},
    {profile:"deepgram-live",result:"pass",batch:null,live:live("standalone-dg-live";"deepgram";"nova-3")},
    {profile:"deepgram-batch-elevenlabs-live",result:"pass",batch:batch("mixed-dg-batch";"deepgram";"nova-3";2),live:live("mixed-el-live";"elevenlabs";"scribe_v2_realtime")},
    {profile:"elevenlabs-batch",result:"pass",batch:batch("standalone-el-batch";"elevenlabs";"scribe_v2";3),live:null},
    {profile:"elevenlabs-live",result:"pass",batch:null,live:live("standalone-el-live";"elevenlabs";"scribe_v2_realtime")},
    {profile:"elevenlabs-batch-deepgram-live",result:"pass",batch:batch("mixed-el-batch";"elevenlabs";"scribe_v2";3),live:live("mixed-dg-live";"deepgram";"nova-3")}
   ]}' >"$acceptance/provider-canary.json"
rebind_approval() {
  canary_sha=$(sha256sum "$acceptance/provider-canary.json" | cut -d' ' -f1)
  campaign_sha=$(sha256sum "$acceptance/campaign-manifest.json" | cut -d' ' -f1)
  fixture_sha=$(sha256sum "$acceptance/fixture-manifest.json" | cut -d' ' -f1)
  policy_sha=$(sha256sum security/release-trust-policy.json | cut -d' ' -f1)
  jq -S -c -n --arg source "$source_sha" --arg image "$image_digest" --arg revision "$runner_revision" \
    --arg canary "$canary_sha" --arg campaign "$campaign_sha" --arg fixtures "$fixture_sha" --arg policy "$policy_sha" '
    {schema_version:1,source_sha:$source,image_digest:$image,campaign_id:"synthetic-verifier-campaign",
     decision:"approved",authorization:"github-environment-required-reviewer",
     protected_environment:"canary-approval",approval_workflow_revision:$source,workflow_run_id:"123456",
     approved_at:"2026-09-02T12:00:00Z",
     runner:{identity:"github.com/777genius/voicetext-canary/.github/workflows/run.yml",revision:$revision},
     canary_payload_sha256:$canary,campaign_manifest_sha256:$campaign,
     fixture_manifest_sha256:$fixtures,trust_policy_sha256:$policy}' >"$acceptance/reviewer-approval.json"
}
rebind_approval
printf '%s\n' '{}' >"$acceptance/reviewer-approval.sigstore.json"
cat >"$sandbox/bin/gh" <<'MOCK'
#!/bin/sh
[ "${MOCK_GH_FAIL:-0}" -eq 0 ] || exit 1
args=" $* "
for required in "--repo 777genius/voicetext-gateway" "--signer-workflow 777genius/voicetext-gateway/.github/workflows/canary-approval.yml" "--signer-digest" "--source-digest" "--deny-self-hosted-runners"; do
  case "$args" in *" $required "*) ;; *) exit 1 ;; esac
done
exit 0
MOCK
chmod +x "$sandbox/bin/gh"
PATH="$sandbox/bin:$PATH"; export PATH

jq -S -c -n --arg source_sha "$source_sha" --arg image_digest "$image_digest" '{bomFormat:"CycloneDX",specVersion:"1.6",components:[{type:"application",name:"voicetext-gateway",version:"test"}],metadata:{properties:[{name:"org.opencontainers.image.revision",value:$source_sha},{name:"org.opencontainers.image.digest",value:$image_digest}]}}' >"$evidence/voicetext-gateway.sbom.cdx.json"
jq -S -c -n '{descriptor:{name:"grype",version:"0.118.0"},matches:[],ignoredMatches:[]}' >"$evidence/vulnerabilities.grype.json"

scripts/verify_release_acceptance.py "$source_sha" "$image_digest" "$evidence"
scripts/verify-release-acceptance.sh "$source_sha" "$image_digest" "$evidence"
scripts/create-release-evidence.sh "$source_sha" ghcr.io/777genius/voicetext-gateway "$image_digest" "$evidence"
scripts/verify-release-evidence.sh "$evidence"

reject_policy() {
  description=$1; file=$2; filter=$3
  cp "$file" "$sandbox/original.json"
  jq -S -c "$filter" "$sandbox/original.json" >"$file"
  rebind_approval
  if scripts/verify_release_acceptance.py "$source_sha" "$image_digest" "$evidence" >/dev/null 2>&1; then
    echo "$description unexpectedly passed" >&2; exit 1
  fi
  mv "$sandbox/original.json" "$file"
  rebind_approval
}
reject_policy "unknown canary key" "$acceptance/provider-canary.json" '.unexpected=true'
reject_policy "partial canary" "$acceptance/provider-canary.json" '.checks=.checks[:-1]'
reject_policy "failed outcome" "$acceptance/provider-canary.json" '.checks[0].batch.outcome="fail"'
reject_policy "mismatched result digest" "$acceptance/provider-canary.json" '.checks[0].batch.result_digest="sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"'
reject_policy "bad fixture binding" "$acceptance/campaign-manifest.json" '.effects[0].fixture_digest="sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"'
reject_policy "duplicate provider operation in same role" "$acceptance/campaign-manifest.json" '.effects[2].provider_operation.id=.effects[0].provider_operation.id'
reject_policy "duplicate provider operation in different role" "$acceptance/campaign-manifest.json" '.effects[7].provider_operation.id=.effects[0].provider_operation.id'
reject_policy "duplicate provider operation in mixed profile" "$acceptance/campaign-manifest.json" '.effects[3].provider_operation.id=.effects[5].provider_operation.id'
reject_policy "operation kind relabel cannot hide reuse" "$acceptance/campaign-manifest.json" '.effects[6].provider_operation.id=.effects[4].provider_operation.id'
reject_policy "duplicate global effect ID" "$acceptance/campaign-manifest.json" '.effects[1].effect_id=.effects[0].effect_id'
reject_policy "wrong operation kind" "$acceptance/campaign-manifest.json" '.effects[0].provider_operation.kind="elevenlabs_transcription_id"'
reject_policy "missing operation kind" "$acceptance/campaign-manifest.json" 'del(.effects[0].provider_operation.kind)'
reject_policy "missing operation ID" "$acceptance/campaign-manifest.json" 'del(.effects[0].provider_operation.id)'
reject_policy "reused mixed effect" "$acceptance/provider-canary.json" '.checks[2].batch=.checks[0].batch'
reject_policy "mismatched campaign digest" "$acceptance/provider-canary.json" '.campaign_manifest_sha256="ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"'
reject_policy "mismatched fixture manifest digest" "$acceptance/provider-canary.json" '.fixture_manifest_sha256="ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"'
reject_policy "mutated campaign source binding" "$acceptance/campaign-manifest.json" '.source_sha="ffffffffffffffffffffffffffffffffffffffff"'
reject_policy "mutated canary source binding" "$acceptance/provider-canary.json" '.source_sha="ffffffffffffffffffffffffffffffffffffffff"'
reject_policy "incomplete live ACK range" "$acceptance/provider-canary.json" '.checks[1].live.ack_last=1'
reject_policy "boolean ACK count" "$acceptance/provider-canary.json" '.checks[1].live.ack_last=true'
reject_policy "wrong latency" "$acceptance/provider-canary.json" '.checks[0].batch.latency_ms=122'
reject_policy "boolean latency" "$acceptance/provider-canary.json" '.checks[0].batch.latency_ms=true'
reject_policy "negative latency" "$acceptance/provider-canary.json" '.checks[0].batch.latency_ms=-1'
reject_policy "reversed timestamps" "$acceptance/provider-canary.json" '.checks[0].batch.completed_at="2026-09-02T10:00:00.949Z"'
reject_policy "ambiguous timezone" "$acceptance/provider-canary.json" '.checks[0].batch.completed_at="2026-09-02T10:00:01.073+00:00"'
reject_policy "unlinked live finalize" "$acceptance/provider-canary.json" '.checks[1].live.finalize.effect_id="different-effect"'
reject_policy "unflushed live finalize" "$acceptance/provider-canary.json" '.checks[1].live.finalize.status="pending"'
reject_policy "inconsistent terminal time" "$acceptance/provider-canary.json" '.checks[1].live.finalize.terminal_at="2026-09-02T10:00:01.074Z"'
reject_policy "unlinked batch terminal" "$acceptance/provider-canary.json" '.checks[0].batch.provider_terminal.provider_operation.id="different-operation"'
reject_policy "mutated terminal digest" "$acceptance/provider-canary.json" '.checks[0].batch.provider_terminal.result_digest="sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"'
reject_policy "mutated finalize digest" "$acceptance/provider-canary.json" '.checks[1].live.finalize.result_digest="sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"'
reject_policy "excess timestamp precision" "$acceptance/provider-canary.json" '.checks[0].batch.completed_at="2026-09-02T10:00:01.0730Z"'
reject_policy "legacy canary schema" "$acceptance/provider-canary.json" '.schema_version=1'
reject_policy "legacy campaign schema" "$acceptance/campaign-manifest.json" '.schema_version=1'
reject_policy "runner mismatch" "$acceptance/provider-canary.json" '.runner.revision="cccccccccccccccccccccccccccccccccccccccc"'

cp "$acceptance/provider-canary.json" "$sandbox/original.json"
python3 - "$acceptance/provider-canary.json" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
value = json.loads(path.read_text(encoding="utf-8"))
value["checks"][0]["batch"]["latency_ms"] = float("nan")
path.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")
PY
rebind_approval
if scripts/verify_release_acceptance.py "$source_sha" "$image_digest" "$evidence" >/dev/null 2>&1; then
  echo "non-finite latency unexpectedly passed" >&2; exit 1
fi
mv "$sandbox/original.json" "$acceptance/provider-canary.json"
rebind_approval

MOCK_GH_FAIL=1; export MOCK_GH_FAIL
if scripts/verify-release-acceptance.sh "$source_sha" "$image_digest" "$evidence" >/dev/null 2>&1; then
  echo "unverifiable approval signature unexpectedly passed" >&2; exit 1
fi
unset MOCK_GH_FAIL

for record in reviewer-approval.json provider-canary.json campaign-manifest.json fixture-manifest.json; do
  cp "$acceptance/$record" "$sandbox/original.json"
  printf '%s\n' '{}' >>"$acceptance/$record"
  if scripts/verify_release_acceptance.py "$source_sha" "$image_digest" "$evidence" >/dev/null 2>&1; then
    echo "multiple JSON documents in $record unexpectedly passed" >&2; exit 1
  fi
  mv "$sandbox/original.json" "$acceptance/$record"
done
cp "$acceptance/provider-canary.json" "$sandbox/original.json"
printf '{"schema_version":1,"schema_version":1}\n' >"$acceptance/provider-canary.json"
if scripts/verify_json_record.py "$acceptance/provider-canary.json" >/dev/null 2>&1; then
  echo "duplicate JSON key unexpectedly passed" >&2; exit 1
fi
mv "$sandbox/original.json" "$acceptance/provider-canary.json"

for record in "$evidence/release-evidence.json" "$evidence/voicetext-gateway.sbom.cdx.json" "$evidence/vulnerabilities.grype.json" "$evidence/policy/vulnerability-policy.json" "$acceptance/reviewer-approval.sigstore.json"; do
  cp "$record" "$sandbox/original.json"
  printf '%s\n' '{}' >>"$record"
  if scripts/verify_json_record.py "$record" >/dev/null 2>&1; then
    echo "multiple JSON documents in $record unexpectedly passed" >&2; exit 1
  fi
  mv "$sandbox/original.json" "$record"
done

echo "synthetic release-evidence verifier tests passed (not real canary evidence)"
