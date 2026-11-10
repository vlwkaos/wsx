#!/bin/sh
# Health check of the local fallback model endpoint. wsx starts no server: the endpoint is whatever
# serves on the URL (kaos's llama-server today, `kaos serve` later). docs/local-model.md.
#   local-model.sh health      exit 0 when the endpoint answers and takes the key
# Settings from the environment:
#   LOCAL_MODEL_URL       base URL, default http://127.0.0.1:8090
#   LOCAL_MODEL_KEY_FILE  API key file, default ~/.local/state/local-model/api-key
set -eu

base=${LOCAL_MODEL_URL:-http://127.0.0.1:8090}
key_file=${LOCAL_MODEL_KEY_FILE:-$HOME/.local/state/local-model/api-key}

case ${1:-} in
  health)
    [ -r "$key_file" ] || { echo "health: key file missing: $key_file"; exit 1; }
    key=$(cat "$key_file")
    # ^ Exit 0 only when /health answers and /v1/models lists a model with the key and answers 401
    # without it. A sleeping llama-server answers /health and wakes on the next chat call.
    curl -sS -m 5 -o /dev/null -f "$base/health" || { echo "health: no answer on $base"; exit 1; }
    id=$(curl -sS -m 5 -f -H "Authorization: Bearer $key" "$base/v1/models" | jq -er '.data[0].id') ||
      { echo "health: /v1/models failed with the key"; exit 1; }
    code=$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "$base/v1/models")
    [ "$code" = 401 ] || { echo "health: /v1/models without a key answered $code, want 401"; exit 1; }
    echo "health: ok ($id)"
    ;;
  *)
    echo "usage: $0 health" >&2
    exit 2
    ;;
esac
