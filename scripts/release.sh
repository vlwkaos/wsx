#!/bin/sh
# tasks.release.command for wsx: publish the version of one main commit (docs/tasks.md "Deploy
# and release"). `wsx task release` runs it on the host in a temporary detached copy of that
# commit with WSX_DEPLOY_COMMIT. The tag starts .github/workflows/release.yml, which runs ci.yml's
# tests, then builds the archive, the GitHub release, wsx-core on crates.io and the Homebrew tap.
# A tag on origin whose version has no GitHub release and no crates.io version moves to the commit.
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
# ^ A rerun of one version: a failed tag run publishes nothing, so origin's tag may move to the fixed
# commit. It moves only while nothing of the version is published: no GitHub release, no crates.io version.
move=
remote=0; found=$(git ls-remote --exit-code --tags origin "refs/tags/$tag" "refs/tags/$tag^{}") || remote=$?
case $remote in
  0)
    old_tag=$(echo "$found" | awk -v ref="refs/tags/$tag" '$2 == ref { print $1 }')
    old_commit=$(echo "$found" | awk -v ref="refs/tags/$tag^{}" '$2 == ref { print $1 }')
    [ -n "$old_tag" ] || fail "cannot read origin's tag $tag"
    [ "${old_commit:-$old_tag}" != "$commit" ] \
      || fail "origin's tag $tag is already on this commit; nothing was published, so rerun its run (gh run rerun --failed)"
    out=$(gh release view "$tag" 2>&1) && fail "$version is released: GitHub has the release $tag; origin's tag stays"
    case $out in *"release not found"*) ;; *) fail "cannot read GitHub release $tag: $out" ;; esac
    code=$(curl -sS -o /dev/null -w '%{http_code}' -A 'wsx-release (github.com/vlwkaos/wsx)' "https://crates.io/api/v1/crates/wsx-core/$version") \
      || fail "cannot read crates.io"
    case $code in
      200) fail "$version is released: crates.io has wsx-core $version; origin's tag stays" ;;
      404) ;;
      *) fail "cannot read crates.io wsx-core $version (HTTP $code)" ;;
    esac
    move=$old_tag
    echo "origin's tag $tag is on ${old_commit:-$old_tag} and $version is not published: the tag moves to $commit"
    ;;
  2) ;;
  *) fail "cannot read origin's tags (git ls-remote exit $remote)" ;;
esac
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null && [ "$(git rev-parse "$tag^{commit}")" = "$commit" ]; then
  echo "tag $tag already exists here on this commit; it is pushed again"
elif git rev-parse -q --verify "refs/tags/$tag" >/dev/null && [ -z "$move" ]; then
  fail "tag $tag exists here on another commit"
fi
origin_main=$(git ls-remote --exit-code origin refs/heads/main | cut -f1) || fail "cannot read origin's main"
git merge-base --is-ancestor "$origin_main" "$commit" 2>/dev/null \
  || fail "$commit does not contain origin/main $origin_main; a push would not fast-forward"
[ -n "$dry" ] || gh auth status >/dev/null 2>&1 || fail "gh is not signed in (gh auth login)"
echo "release $tag of $commit over origin/main $origin_main"

# ^ The live handoff from the last installed release (docs/daemon-live-upgrade.md), which a task
# cannot run inside wsx's boundary. It opens PTYs and daemons, so it takes the scenario test slot.
step "handoff from the installed release"
act cargo build --locked -p wsx -p wsx-daemon
act sh scripts/terminal-context-check.sh "$version"

step "push main"
act git push origin "$commit:refs/heads/main"

# ^ The push run of workflow $1 on this exact commit; watch_run follows it to its end.
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
watch_run() {
  [ -n "$dry" ] && return 0
  echo "watching $(gh run view "$1" --json url --jq .url)"
  gh run watch "$1" --exit-status --interval 30 >/dev/null 2>&1
}

step "push tag"
# ^ Moving the tag leases only this tag at the value read in the preflight.
[ "$(git rev-parse -q --verify "refs/tags/$tag^{commit}" || true)" = "$commit" ] || act git tag -fa "$tag" -m "wsx $version" "$commit"
if [ -n "$move" ]; then
  act git push --force-with-lease="refs/tags/$tag:$move" origin "refs/tags/$tag"
else
  act git push origin "refs/tags/$tag"
fi

step "release.yml"
run=$(run_of release.yml)
if ! watch_run "$run"; then
  failed=$(gh run view "$run" --json jobs --jq '.jobs[].steps[] | select(.conclusion == "failure") | .name')
  echo "release.yml failed at: $failed"
  # ^ release.yml's first job runs ci.yml's tests; its jobs read "Tests / <os>" and nothing is built yet.
  tests=$(gh run view "$run" --json jobs --jq '.jobs[] | select(.conclusion == "failure") | .name | select(startswith("Tests / "))')
  if [ -n "$tests" ]; then
    gh run view "$run" --log-failed | tail -40
    echo "NOT PUBLISHED: tests failed for $tag: $(echo $tests)"
    echo "The tag is on origin, but nothing was built or published. Fix the failure on main and run the release"
    echo "again: it moves the tag to the new commit while $version has no GitHub release and no crates.io version."
    exit 1
  fi
  # ^ Credentials are checked in release.yml's preflight job, before anything is published.
  if [ -n "$failed" ] && ! echo "$failed" | grep -Evx 'Verify Homebrew tap access|Verify crates.io Trusted Publisher|Authenticate to crates.io' >/dev/null; then
    echo "NOT PUBLISHED: $tag is pushed, but release.yml stopped at a credential step: $failed"
    echo "Fix the TAP_TOKEN secret or the crates.io Trusted Publisher, then rerun: gh run rerun $run --failed"
    exit 1
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
