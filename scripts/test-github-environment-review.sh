#!/bin/sh
# shellcheck disable=SC2089,SC2090
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
sandbox=$(mktemp -d "${TMPDIR:-/tmp}/voicetext-environment-review-test.XXXXXX")
cleanup() { find "$sandbox" -depth -delete; }
trap cleanup EXIT INT TERM
mkdir "$sandbox/bin"

cat >"$sandbox/bin/gh" <<'MOCK'
#!/bin/sh
printf '%s\n' "$*" >>"$MOCK_GH_LOG"
[ "${MOCK_GH_FAIL:-0}" -eq 0 ] || exit 1
case "$*" in
  *'/repos/777genius/voicetext-gateway/environments/'*)
    printf '%s\n' "$MOCK_ENVIRONMENT_RESPONSE"
    ;;
  *'/repos/777genius/voicetext-gateway/actions/runs/'*'/approvals'*)
    printf '%s\n' "$MOCK_APPROVAL_RESPONSE"
    ;;
  *)
    echo "unexpected fake-gh request: $*" >&2
    exit 2
    ;;
esac
MOCK
chmod +x "$sandbox/bin/gh"
PATH="$sandbox/bin:$PATH"
MOCK_GH_LOG="$sandbox/gh.log"
export PATH MOCK_GH_LOG

valid_environment='{"name":"canary-approval","id":7001,"protection_rules":[{"type":"required_reviewers","prevent_self_review":true,"reviewers":[{"type":"User","reviewer":{"id":202,"login":"bob"}}]}]}'
valid_approval='[{"state":"approved","comment":"approved for synthetic test","environments":[{"id":7001,"name":"canary-approval"}],"user":{"id":202,"login":"bob","type":"User"}}]'

run_guard() {
  environment=${1:-canary-approval}
  attempt=${2:-1}
  actor=${3:-alice}
  actor_id=${4:-101}
  scripts/verify-github-environment-review.py \
    777genius/voicetext-gateway "$environment" 12345 "$attempt" "$actor" "$actor_id"
}

authorize_then_effect() {
  if run_guard "$@" >"$sandbox/stdout" 2>"$sandbox/stderr"; then
    printf 'publication-effect\n' >>"$sandbox/effects"
    return 0
  fi
  return 1
}

expect_accept() {
  description=$1
  shift
  : >"$sandbox/effects"
  if ! authorize_then_effect "$@"; then
    echo "$description unexpectedly failed" >&2
    cat "$sandbox/stderr" >&2
    exit 1
  fi
  test "$(cat "$sandbox/effects")" = publication-effect
}

expect_reject() {
  description=$1
  shift
  : >"$sandbox/effects"
  if authorize_then_effect "$@"; then
    echo "$description unexpectedly passed" >&2
    exit 1
  fi
  if test -s "$sandbox/effects"; then
    echo "$description reached a downstream effect" >&2
    exit 1
  fi
}

MOCK_ENVIRONMENT_RESPONSE=$valid_environment
MOCK_APPROVAL_RESPONSE=$valid_approval
export MOCK_ENVIRONMENT_RESPONSE MOCK_APPROVAL_RESPONSE
expect_accept "valid independent human approval"
test "$(wc -l <"$MOCK_GH_LOG")" -eq 2

MOCK_ENVIRONMENT_RESPONSE='{"name":"release-publication","id":8001,"protection_rules":[{"type":"required_reviewers","prevent_self_review":true,"reviewers":[{"type":"Team","reviewer":{"id":303,"slug":"release-reviewers"}}]}]}'
MOCK_APPROVAL_RESPONSE='[{"state":"approved","environments":[{"id":8001,"name":"release-publication"}],"user":{"id":202,"login":"bob","type":"User"}}]'
export MOCK_ENVIRONMENT_RESPONSE MOCK_APPROVAL_RESPONSE
expect_accept "valid team-authorized publication review" release-publication

MOCK_ENVIRONMENT_RESPONSE=$valid_environment
MOCK_APPROVAL_RESPONSE=$valid_approval
export MOCK_ENVIRONMENT_RESPONSE MOCK_APPROVAL_RESPONSE
expect_reject "rerun ambiguity" canary-approval 2
grep -F "reruns are refused" "$sandbox/stderr" >/dev/null
expect_reject "self approval by login" canary-approval 1 BOB 999
expect_reject "self approval by id" canary-approval 1 different-login 202

MOCK_ENVIRONMENT_RESPONSE='{"name":"wrong","id":7001,"protection_rules":[]}'
export MOCK_ENVIRONMENT_RESPONSE
expect_reject "wrong configured environment"
MOCK_ENVIRONMENT_RESPONSE='{"name":"canary-approval","id":7001,"protection_rules":[]}'
export MOCK_ENVIRONMENT_RESPONSE
expect_reject "missing protection rule"
MOCK_ENVIRONMENT_RESPONSE='{"name":"canary-approval","id":7001,"protection_rules":[{"type":"required_reviewers","prevent_self_review":false,"reviewers":[{"type":"User","reviewer":{"id":202}}]}]}'
export MOCK_ENVIRONMENT_RESPONSE
expect_reject "self review is not prevented"
MOCK_ENVIRONMENT_RESPONSE='{"name":"canary-approval","id":7001,"protection_rules":[{"type":"required_reviewers","prevent_self_review":true,"reviewers":[]}]}'
export MOCK_ENVIRONMENT_RESPONSE
expect_reject "empty required reviewers"
MOCK_ENVIRONMENT_RESPONSE='{"name":"canary-approval","id":7001,"protection_rules":[{"type":"required_reviewers","prevent_self_review":true,"reviewers":[{"type":"Bot","reviewer":{"id":202}}]}]}'
export MOCK_ENVIRONMENT_RESPONSE
expect_reject "invalid configured reviewer"
MOCK_ENVIRONMENT_RESPONSE='{"name":"canary-approval","id":7001,"protection_rules":[{"type":"required_reviewers","prevent_self_review":true,"reviewers":[{"type":"User","reviewer":{"id":202}}]},{"type":"required_reviewers","prevent_self_review":true,"reviewers":[{"type":"User","reviewer":{"id":203}}]}]}'
export MOCK_ENVIRONMENT_RESPONSE
expect_reject "ambiguous required-reviewer rules"
MOCK_ENVIRONMENT_RESPONSE=''
export MOCK_ENVIRONMENT_RESPONSE
expect_reject "empty environment API response"
MOCK_ENVIRONMENT_RESPONSE='{"name":"canary-approval",'
export MOCK_ENVIRONMENT_RESPONSE
expect_reject "invalid environment JSON"
MOCK_ENVIRONMENT_RESPONSE='{"name":"canary-approval","name":"canary-approval","id":7001,"protection_rules":[]}'
export MOCK_ENVIRONMENT_RESPONSE
expect_reject "duplicate environment JSON key"

MOCK_ENVIRONMENT_RESPONSE=$valid_environment
MOCK_APPROVAL_RESPONSE='[]'
export MOCK_ENVIRONMENT_RESPONSE MOCK_APPROVAL_RESPONSE
expect_reject "empty review history"
MOCK_APPROVAL_RESPONSE='[{"state":"approved","environments":[{"id":7002,"name":"canary-approval"}],"user":{"id":202,"login":"bob","type":"User"}}]'
export MOCK_APPROVAL_RESPONSE
expect_reject "wrong reviewed environment id"
MOCK_APPROVAL_RESPONSE='[{"state":"approved","environments":[{"id":7001,"name":"wrong"}],"user":{"id":202,"login":"bob","type":"User"}}]'
export MOCK_APPROVAL_RESPONSE
expect_reject "wrong reviewed environment name"
MOCK_APPROVAL_RESPONSE='[{"state":"rejected","environments":[{"id":7001,"name":"canary-approval"}],"user":{"id":202,"login":"bob","type":"User"}}]'
export MOCK_APPROVAL_RESPONSE
expect_reject "rejected review"
MOCK_APPROVAL_RESPONSE='[{"state":"approved","environments":[{"id":7001,"name":"canary-approval"}],"user":{"id":202,"login":"bob","type":"Bot"}}]'
export MOCK_APPROVAL_RESPONSE
expect_reject "bot approval type"
MOCK_APPROVAL_RESPONSE='[{"state":"approved","environments":[{"id":7001,"name":"canary-approval"}],"user":{"id":202,"login":"release[bot]","type":"User"}}]'
export MOCK_APPROVAL_RESPONSE
expect_reject "bot approval login"
MOCK_APPROVAL_RESPONSE="[$valid_approval" # invalid nested-array JSON is unreadable evidence
export MOCK_APPROVAL_RESPONSE
expect_reject "invalid review JSON"
MOCK_APPROVAL_RESPONSE='[{"state":"approved","environments":[{"id":7001,"name":"canary-approval"}],"user":{"id":202,"login":"bob","type":"User"}},{"state":"approved","environments":[{"id":7001,"name":"canary-approval"}],"user":{"id":203,"login":"carol","type":"User"}}]'
export MOCK_APPROVAL_RESPONSE
expect_reject "ambiguous multiple approvals"

MOCK_APPROVAL_RESPONSE=$valid_approval
MOCK_GH_FAIL=1
export MOCK_APPROVAL_RESPONSE MOCK_GH_FAIL
expect_reject "GitHub API failure"
unset MOCK_GH_FAIL

: >"$sandbox/effects"
if scripts/verify-github-environment-review.py other/repository canary-approval 12345 1 alice 101 \
  >"$sandbox/stdout" 2>"$sandbox/stderr"; then
  echo "wrong repository unexpectedly passed" >&2
  exit 1
fi
test ! -s "$sandbox/effects"

echo "fake-gh environment review authorization tests passed"
