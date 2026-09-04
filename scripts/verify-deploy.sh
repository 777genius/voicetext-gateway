#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

fail() {
  echo "deploy verification failed: $*" >&2
  exit 1
}

frontend='# syntax=docker/dockerfile:1.18@sha256:dabfc0969b935b2080555ace70ee69a5261af8a8f1b4df97b9e7fbcf6722eddf'
[ "$(sed -n '1p' Dockerfile)" = "$frontend" ] || fail "Dockerfile frontend is not the reviewed 1.18 digest"
[ "$(sha256sum LICENSE | cut -d' ' -f1)" = \
  c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 ] ||
  fail "LICENSE is not the reviewed Apache License 2.0 text"

if awk '/^FROM / && $2 != "builder" && $0 !~ /@sha256:[0-9a-f]{64}/ { bad = 1 } END { exit bad }' Dockerfile; then
  :
else
  fail "every Dockerfile base image must be pinned by digest"
fi

grep -Fq 'VOICETEXT_HEALTHCHECK_URL=http://127.0.0.1:8080/health/ready' Dockerfile ||
  fail "image healthcheck default is missing"
grep -Fq '${VOICETEXT_HEALTHCHECK_URL}' Dockerfile ||
  fail "image healthcheck does not honor its configured URL"
grep -Eq 'remote_context=.*\.git\?ref=.*&checksum=' .github/workflows/ci.yml ||
  fail "CI does not exercise a remote Git ref+checksum build context"
grep -Fq 'COPY --chmod=0444 LICENSE NOTICE /usr/share/licenses/voicetext-gateway/' Dockerfile ||
  fail "runtime image does not package Apache-2.0 license and notice"
grep -Fq 'org.opencontainers.image.revision="${SOURCE_SHA}"' Dockerfile ||
  fail "runtime image does not carry the exact source revision label"

for route in \
  /api/v1/transcribe/batch \
  '/api/v1/transcribe/batch/*' \
  /api/v1/transcribe/stream \
  /health/live
do
  grep -Fq "$route" deploy/Caddyfile || fail "Caddy contract matcher omits $route"
done

if awk '$1 == "/health" || $1 == "/health/ready" { bad = 1 } END { exit !bad }' deploy/Caddyfile; then
  fail "Caddy must keep dependency health routes internal"
fi

proxy_line=$(grep -n '^[[:space:]]*handle @voicetext_contract {' deploy/Caddyfile | cut -d: -f1)
fallback_line=$(grep -n '^[[:space:]]*handle {$' deploy/Caddyfile | cut -d: -f1)
[ -n "$proxy_line" ] && [ -n "$fallback_line" ] && [ "$proxy_line" -lt "$fallback_line" ] ||
  fail "Caddy proxy and fallback must be ordered handle blocks"

compose_json=$(mktemp)
adapted_json=$(mktemp)
trap 'rm -f -- "$compose_json" "$adapted_json"' EXIT HUP INT TERM
if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  compose_files='-f deploy/compose.yaml'
  VOICETEXT_PUBLIC_HOST=voice.example.invalid \
    docker compose --env-file deploy/.env.example $compose_files config --quiet
  VOICETEXT_PUBLIC_HOST=voice.example.invalid \
    docker compose --env-file deploy/.env.example $compose_files \
      -f deploy/compose.deepgram.yaml config --quiet
  VOICETEXT_PUBLIC_HOST=voice.example.invalid \
    docker compose --env-file deploy/.env.example $compose_files \
      -f deploy/compose.elevenlabs.yaml config --quiet
  VOICETEXT_PUBLIC_HOST=voice.example.invalid \
    docker compose --env-file deploy/.env.example $compose_files \
      -f deploy/compose.tls.yaml config --quiet
  VOICETEXT_PUBLIC_HOST=voice.example.invalid \
    docker compose --env-file deploy/.env.example -f deploy/compose.yaml \
      -f deploy/compose.tls.yaml config --format json >"$compose_json"

  jq -e '
    .services.gateway as $g |
    .services["secret-init"] as $i |
    ($g.read_only == true) and
    ($g.init == true) and
    ($g.pids_limit <= 256) and
    ($g.cap_drop == ["ALL"]) and
    ($g.security_opt | index("no-new-privileges:true") != null) and
    ($g.volumes | any(.target == "/run/secrets" and .read_only == true)) and
    ($i.network_mode == "none") and
    ($i.read_only == true) and
    ($i.healthcheck.disable == true) and
    ($i.cap_drop == ["ALL"]) and
    ($i.cap_add | sort == ["CHOWN", "DAC_OVERRIDE", "FOWNER"]) and
    ($i.environment | tostring | contains("gateway_token")) and
    ($g.environment.VOICETEXT_BEARER_TOKEN_FILE == "/run/secrets/gateway_token") and
    ($g.environment.VOICETEXT_HEALTHCHECK_URL == "http://127.0.0.1:8080/health/ready") and
    (($g.environment | keys | map(select(test("(TOKEN|API_KEY)$"))) | length) == 0)
  ' "$compose_json" >/dev/null || fail "Compose runtime or secret-custody hardening regressed"
else
  for required in 'read_only: true' 'no-new-privileges:true' 'network_mode: none' \
    'VOICETEXT_BEARER_TOKEN_FILE: /run/secrets/gateway_token' \
    'VOICETEXT_HEALTHCHECK_URL:' 'source: gateway-secrets' 'target: /run/secrets'
  do
    grep -Fq "$required" deploy/compose.yaml || fail "static Compose check is missing $required"
  done
  echo "Docker Compose unavailable; deterministic manifest checks applied"
fi

caddy_image='caddy:2.10-alpine@sha256:4c6e91c6ed0e2fa03efd5b44747b625fec79bc9cd06ac5235a779726618e530d'
if command -v caddy >/dev/null 2>&1; then
  VOICETEXT_PUBLIC_HOST=voice.example.invalid caddy adapt \
    --config deploy/Caddyfile --adapter caddyfile >"$adapted_json"
elif command -v docker >/dev/null 2>&1 && docker image inspect "$caddy_image" >/dev/null 2>&1; then
  docker run --rm --network none --entrypoint caddy \
    -e VOICETEXT_PUBLIC_HOST=voice.example.invalid \
    -v "$root/deploy/Caddyfile:/etc/caddy/Caddyfile:ro" \
    "$caddy_image" adapt --config /etc/caddy/Caddyfile --adapter caddyfile >"$adapted_json"
else
  echo "Caddy image unavailable offline; deterministic handle-order checks applied"
fi

if [ -s "$adapted_json" ]; then
  jq -e '
    (tostring | contains("/api/v1/transcribe/batch")) and
    (tostring | contains("/api/v1/transcribe/stream")) and
    ([paths(objects | select(.handler? == "reverse_proxy"))][0] as $proxy |
     [paths(objects | select(.handler? == "static_response" and .status_code? == 404))][0] as $fallback |
     ($proxy != null and $fallback != null and $proxy < $fallback))
  ' "$adapted_json" >/dev/null || fail "adapted Caddy routes do not put the contract proxy before 404"
fi

git check-ignore -q deploy/.env || fail "deploy/.env is not ignored"
git ls-files --error-unmatch deploy/.env.example >/dev/null || fail "deploy/.env.example is not tracked"

echo "deploy verification passed"
