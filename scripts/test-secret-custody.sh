#!/bin/sh
set -eu

[ "$#" -eq 1 ] || {
  echo "usage: $0 IMAGE" >&2
  exit 2
}
image=$1
fixture_dir=$(mktemp -d)
volume="voicetext-secret-test-$$"

cleanup() {
  docker volume rm --force "$volume" >/dev/null 2>&1 || true
  find "$fixture_dir" -type f -delete
  rmdir "$fixture_dir"
}
trap cleanup EXIT HUP INT TERM

umask 077
printf '%s\n' 'synthetic-service-token-not-a-credential' >"$fixture_dir/gateway_token"
printf '%s\n' 'synthetic-provider-key-not-a-credential' >"$fixture_dir/deepgram_api_key"
chmod 0600 "$fixture_dir/gateway_token" "$fixture_dir/deepgram_api_key"
docker volume create "$volume" >/dev/null

docker run --rm --network none --read-only --user 0:0 \
  --cap-drop ALL --cap-add CHOWN --cap-add DAC_OVERRIDE --cap-add FOWNER \
  --entrypoint /bin/sh \
  -e 'VOICETEXT_SECRET_FILES=gateway_token deepgram_api_key' \
  -v "$fixture_dir:/source:ro" -v "$volume:/secrets" \
  "$image" -ec '
    umask 077
    rm -f /secrets/gateway_token /secrets/deepgram_api_key /secrets/elevenlabs_api_key
    for name in ${VOICETEXT_SECRET_FILES}; do
      install -o 10001 -g 10001 -m 0400 "/source/${name}" "/secrets/${name}"
    done
  '

docker run --rm --network none --read-only --user 10001:10001 \
  --cap-drop ALL --security-opt no-new-privileges \
  --entrypoint /bin/sh -v "$volume:/run/secrets:ro" "$image" -ec '
    test -r /run/secrets/gateway_token
    test -r /run/secrets/deepgram_api_key
    test "$(stat -c %u:%g:%a /run/secrets/gateway_token)" = 10001:10001:400
    test "$(stat -c %u:%g:%a /run/secrets/deepgram_api_key)" = 10001:10001:400
    test ! -e /run/secrets/elevenlabs_api_key
  '

docker run --rm --network none --read-only --user 10002:10002 \
  --cap-drop ALL --security-opt no-new-privileges \
  --entrypoint /bin/sh -v "$volume:/run/secrets:ro" "$image" -ec '
    test ! -r /run/secrets/gateway_token
    test ! -r /run/secrets/deepgram_api_key
  '

echo "secret custody verification passed"
