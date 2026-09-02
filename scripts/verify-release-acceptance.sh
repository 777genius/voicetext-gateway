#!/bin/sh
set -eu

[ "$#" -eq 3 ] || {
  echo "usage: $0 SOURCE_SHA IMAGE_DIGEST EVIDENCE_DIRECTORY" >&2
  exit 2
}
source_sha=$1
image_digest=$2
evidence_dir=$3
reviewer="$evidence_dir/acceptance/reviewer-approval.json"
canary="$evidence_dir/acceptance/provider-canary.json"

for required in "$reviewer" "$canary"; do
  [ -s "$required" ] || {
    echo "missing protected acceptance evidence: $required" >&2
    exit 1
  }
  [ "$(wc -c <"$required")" -le 65536 ] || {
    echo "oversized protected acceptance evidence: $required" >&2
    exit 1
  }
done

jq -e --arg source_sha "$source_sha" --arg image_digest "$image_digest" '
  def exact_keys($names): (keys | sort) == ($names | sort);
  def timestamp: type == "string" and
    test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$") and
    ((try fromdateiso8601 catch null) | type == "number");
  exact_keys(["source_sha", "image_digest", "decision", "reviewer_login", "approved_at"]) and
  .source_sha == $source_sha and .image_digest == $image_digest and
  (.source_sha | test("^[0-9a-f]{40}$")) and
  (.image_digest | test("^sha256:[0-9a-f]{64}$")) and
  .decision == "approved" and
  (.reviewer_login | type == "string" and test("^[A-Za-z0-9-]{1,39}$")) and
  (.approved_at | timestamp)
' "$reviewer" >/dev/null

jq -e --arg source_sha "$source_sha" --arg image_digest "$image_digest" '
  def exact_keys($names): (keys | sort) == ($names | sort);
  def bounded_id: type == "string" and
    test("^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,127}$");
  def digest: type == "string" and test("^sha256:[0-9a-f]{64}$");
  def timestamp: type == "string" and
    test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$") and
    ((try fromdateiso8601 catch null) | type == "number");
  def base_evidence($provider; $model; $mode; $version):
    exact_keys(["provider", "model", "mode", "contract_version", "language",
      "fixture_digest", "result_digest", "request_id", "result_id", "started_at",
      "completed_at", "latency_ms", "error_classification", "credential_owner"]) and
    .provider == $provider and .model == $model and .mode == $mode and
    .contract_version == $version and .language == "multi" and
    (.fixture_digest | digest) and (.result_digest | digest) and
    (.request_id | bounded_id) and (.result_id | bounded_id) and
    (.started_at | timestamp) and (.completed_at | timestamp) and
    .started_at <= .completed_at and
    (.latency_ms | type == "number" and floor == . and . >= 0 and . <= 86400000) and
    ((.completed_at | fromdateiso8601) - (.started_at | fromdateiso8601)) * 1000 == .latency_ms and
    .error_classification == "none" and (.credential_owner | bounded_id);
  def batch($provider; $model; $version):
    base_evidence($provider; $model; "batch"; $version);
  def live($provider; $model):
    exact_keys(["provider", "model", "mode", "contract_version", "language",
      "fixture_digest", "result_digest", "request_id", "result_id", "started_at",
      "completed_at", "latency_ms", "error_classification", "credential_owner",
      "ack_count", "finalize_status"]) and . as $e |
    del(.ack_count, .finalize_status) |
      base_evidence($provider; $model; "live"; 2) and
      ($e.ack_count | type == "number" and floor == . and . > 0) and
      $e.finalize_status == "observed";
  def check_shape:
    exact_keys(["profile", "result", "batch", "live"] ) and .result == "pass";
  def valid_check:
    check_shape and
    if .profile == "deepgram-batch" then
      (.batch | batch("deepgram"; "nova-3"; 2)) and .live == null
    elif .profile == "elevenlabs-batch" then
      (.batch | batch("elevenlabs"; "scribe_v2"; 3)) and .live == null
    elif .profile == "deepgram-live" then
      .batch == null and (.live | live("deepgram"; "nova-3"))
    elif .profile == "elevenlabs-live" then
      .batch == null and (.live | live("elevenlabs"; "scribe_v2_realtime"))
    elif .profile == "deepgram-batch-elevenlabs-live" then
      (.batch | batch("deepgram"; "nova-3"; 2)) and
      (.live | live("elevenlabs"; "scribe_v2_realtime"))
    elif .profile == "elevenlabs-batch-deepgram-live" then
      (.batch | batch("elevenlabs"; "scribe_v2"; 3)) and
      (.live | live("deepgram"; "nova-3"))
    else false end;
  exact_keys(["source_sha", "image_digest", "result", "campaign_id", "campaign_owner",
    "campaign_approval", "completed_at", "checks"]) and
  .source_sha == $source_sha and .image_digest == $image_digest and .result == "pass" and
  (.source_sha | test("^[0-9a-f]{40}$")) and
  (.image_digest | digest) and
  (.campaign_id | bounded_id) and (.campaign_owner | bounded_id) and
  (.campaign_approval | bounded_id) and (.completed_at | timestamp) and
  (.checks | type == "array" and length == 6) and
  ([.checks[].profile] | sort) ==
    (["deepgram-batch", "deepgram-live", "deepgram-batch-elevenlabs-live",
      "elevenlabs-batch", "elevenlabs-live", "elevenlabs-batch-deepgram-live"] | sort) and
  all(.checks[]; valid_check and
    ((.batch == null or .batch.completed_at <= $completed_at) and
     (.live == null or .live.completed_at <= $completed_at)))
' --arg completed_at "$(jq -er '.completed_at' "$canary")" "$canary" >/dev/null
