#!/bin/sh
# tasks.release.command for wsx: publish the version of one main commit (docs/tasks.md "Deploy
# and release"). `wsx task release` runs it on the host in a temporary detached copy of that
# commit with WSX_DEPLOY_COMMIT. The tag starts .github/workflows/release.yml, which builds the
# archive, the GitHub release, wsx-core on crates.io and the Homebrew tap.
# WSX_RELEASE_DRY_RUN=1 runs every check and prints the builds, pushes and waits instead.
set -eu
commit=$(git rev-parse --verify "${WSX_DEPLOY_COMMIT:-HEAD}^{commit}")
dry=${WSX_RELEASE_DRY_RUN:-}
step=start
step() { step=$1; echo "== release step: $1 ($(date '+%H:%M:%S'))"; }
trap 'code=$?; [ $code -eq 0 ] || echo "release failed at step: $step (exit $code)"' EXIT
fail() { echo "$*" >&2; exit 1; }
act() {
  if [ -n "$dry" ]; then echo "dry run: $*"; else "$@"; fi
}

step preflight
# ^ The same version source and tag shape as release.yml "Validate release tag".
version=$(git show "$commit:Cargo.toml" | awk -F '"' '/^version =/ { print $2; exit }')
tag="v$version"
echo "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || fail "version '$version' is not a stable release (release.yml takes vX.Y.Z)"
git show "$commit:CHANGELOG.md" | grep -q "^## \[$version\]" || fail "CHANGELOG.md has no [$version] section"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  [ "$(git rev-parse "$tag^{commit}")" = "$commit" ] || fail "tag $tag exists here on another commit"
  echo "tag $tag already exists here on this commit; it is pushed again"
fi
remote=0; git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null || remote=$?
case $remote in
  0) fail "origin already has tag $tag: $version is released" ;;
  2) ;;
  *) fail "cannot read origin's tags (git ls-remote exit $remote)" ;;
esac
origin_main=$(git ls-remote --exit-code origin refs/heads/main | cut -f1) || fail "cannot read origin's main"
git merge-base --is-ancestor "$origin_main" "$commit" 2>/dev/null \
  || fail "$commit does not contain origin/main $origin_main; a push would not fast-forward"
[ -n "$dry" ] || gh auth status >/dev/null 2>&1 || fail "gh is not signed in (gh auth login)"
echo "release $tag of $commit over origin/main $origin_main"

# ^ The live handoff from the last installed release (docs/daemon-live-upgrade.md), which a task
# cannot run inside wsx's boundary. It opens PTYs and daemons, so it takes the check slot.
step "handoff from the installed release"
cellar=$(brew --cellar wsx 2>/dev/null || true)
older=$( { [ -n "$cellar" ] && ls "$cellar" 2>/dev/null || true; } | grep -Fvx "$version" \
  | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)
if [ -n "$older" ] && [ -x "$cellar/$older/bin/wsx" ] && [ -x "$cellar/$older/bin/wsxd" ]; then
  act cargo build --locked -p wsx -p wsx-daemon
  act python3 -B scripts/check-slot.py python3 -B scripts/test-terminal-context.py \
    --released-wsx "$cellar/$older/bin/wsx" --released-daemon "$cellar/$older/bin/wsxd"
else
  echo "skipped: no older wsx release is installed under Homebrew (${cellar:-no brew})"
fi

step "push main"
act git push origin "$commit:refs/heads/main"

# ^ Wait for the workflow run of `event` on this exact commit and watch it to its end.
run_of() {
  [ -n "$dry" ] && { echo "dry run: wait for $1 on $commit" >&2; echo dry; return; }
  i=0
  while :; do
    id=$(gh run list --workflow "$1" --commit "$commit" --event push --limit 1 \
      --json databaseId --jq '.[0].databaseId // empty')
    [ -n "$id" ] && { echo "$id"; return; }
    i=$((i+1)); [ $i -lt 60 ] || fail "no $1 run for $commit after 5 minutes"
    sleep 5
  done
}
watch() {
  [ -n "$dry" ] && return 0
  echo "watching $(gh run view "$1" --json url --jq .url)"
  gh run watch "$1" --exit-status --interval 30 >/dev/null 2>&1
}

step "ci.yml"
ci=$(run_of ci.yml)
watch "$ci" || { gh run view "$ci" --log-failed | tail -40; fail "ci.yml failed on $commit"; }

step "push tag"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null || act git tag -a "$tag" -m "wsx $version" "$commit"
act git push origin "refs/tags/$tag"

step "release.yml"
run=$(run_of release.yml)
if ! watch "$run"; then
  failed=$(gh run view "$run" --json jobs --jq '.jobs[].steps[] | select(.conclusion == "failure") | .name')
  echo "release.yml failed at: $failed"
  # ^ Credentials are checked in release.yml's preflight job, before anything is published.
  if [ -n "$failed" ] && ! echo "$failed" | grep -Evx 'Verify Homebrew tap access|Verify crates.io Trusted Publisher|Authenticate to crates.io' >/dev/null; then
    echo "NOT PUBLISHED: $tag is pushed, but release.yml stopped at a credential step: $failed"
    echo "Fix the TAP_TOKEN secret or the crates.io Trusted Publisher, then rerun: gh run rerun $run --failed"
    exit 0
  fi
  gh run view "$run" --log-failed | tail -40
  fail "release.yml failed for $tag"
fi

step "published"
[ -n "$dry" ] && { echo "dry run: report the GitHub release, crates.io wsx-core $version and the tap"; exit 0; }
echo "GitHub release: $(gh release view "$tag" --json url --jq .url)"
if curl -fsS -A 'wsx-release (github.com/vlwkaos/wsx)' "https://crates.io/api/v1/crates/wsx-core/$version" >/dev/null; then
  echo "crates.io: wsx-core $version"
else
  echo "crates.io: wsx-core $version is not there yet"
fi
if gh api repos/vlwkaos/homebrew-tap/contents/Formula/wsx.rb --jq .content | base64 -d | grep -Fq "$tag"; then
  echo "Homebrew tap: Formula/wsx.rb has $tag"
else
  echo "Homebrew tap: Formula/wsx.rb does not have $tag yet"
fi
