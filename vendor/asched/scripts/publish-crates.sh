#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:?usage: publish-crates.sh VERSION}"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "invalid release version: ${VERSION}" >&2
  exit 2
fi

published() {
  curl --fail --silent --show-error --output /dev/null \
    --user-agent "asched-release/${VERSION}" \
    "https://crates.io/api/v1/crates/$1/${VERSION}"
}

if ! published asched-core; then
  cargo publish -p asched-core --locked
fi

if published asched; then
  exit 0
fi

for attempt in $(seq 1 12); do
  if cargo publish -p asched --locked; then
    exit 0
  fi
  if published asched; then
    exit 0
  fi
  if [[ "$attempt" -lt 12 ]]; then
    sleep 10
  fi
done

echo "asched did not publish after waiting for the asched-core index update" >&2
exit 1
