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
  .source_sha == $source_sha and .image_digest == $image_digest and
  .decision == "approved" and (.reviewer_login | test("^[A-Za-z0-9-]{1,39}$")) and
  (.approved_at | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T"))
' "$reviewer" >/dev/null

jq -e --arg source_sha "$source_sha" --arg image_digest "$image_digest" '
  .source_sha == $source_sha and .image_digest == $image_digest and .result == "pass" and
  (.campaign_id | type == "string" and length > 0 and length <= 128) and
  (.completed_at | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T")) and
  (.checks | type == "array" and length == 6) and
  ([.checks[] | select(.result == "pass") | .profile] | unique | sort) ==
    (["deepgram-batch", "deepgram-live", "deepgram-batch-elevenlabs-live",
      "elevenlabs-batch", "elevenlabs-live", "elevenlabs-batch-deepgram-live"] | sort)
' "$canary" >/dev/null
