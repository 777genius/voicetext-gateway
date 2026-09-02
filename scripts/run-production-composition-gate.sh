#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
fixture_dir="$root/contract-fixtures/typescript-consumer"

(cd "$fixture_dir" && sha256sum --check SHA256SUMS)
node_major=$(node --version | sed 's/^v//' | cut -d. -f1)
[ "$node_major" -ge 24 ] || {
  echo "production composition gate requires Node.js 24 or newer" >&2
  exit 1
}

: "${VOICETEXT_TEST_DATABASE_URL:?must identify a disposable voicetext_test_* database}"
production_binary=${VOICETEXT_GATEWAY_PRODUCTION_BINARY:-"$root/target/release/voicetext-gateway"}
[ -x "$production_binary" ] || {
  echo "missing release-mode gateway binary: $production_binary" >&2
  exit 1
}

VOICETEXT_GATEWAY_PRODUCTION_BINARY="$production_binary" \
  cargo test --locked -p voicetext-gateway --test production_composition_e2e \
  production_binary_matches_the_typescript_consumer_through_real_provider_adapters \
  -- --ignored --exact
