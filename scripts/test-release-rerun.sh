#!/bin/sh
# Scenario for scripts/release.sh's rerun rule (docs/tasks.md "Deploy and release"): a version tag on
# origin moves to the new commit while nothing of the version is published, and stays otherwise.
# Real git against a scratch bare origin; only gh, curl and cargo are stubs. Pushes nowhere else.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(mktemp -d "${TMPDIR:-/tmp}/wsx-release-rerun.XXXXXX")
trap 'rm -rf "$root"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
mkdir "$root/bin"
cat > "$root/bin/gh" <<'S'
#!/bin/sh
case "$1 $2" in
  "auth status"|"run watch") exit 0 ;;
  "run list") echo 1 ;;
  "run view") echo https://example.invalid/run/1 ;;
  "release view") if [ -n "${STUB_RELEASE:-}" ]; then echo https://example.invalid/release; else echo "release not found" >&2; exit 1; fi ;;
  *) exit 1 ;;
esac
S
cat > "$root/bin/curl" <<'S'
#!/bin/sh
code=${STUB_CRATES:-404}
# ^ The release step reads with -f (fails on 404); the preflight reads the code with -w.
case " $* " in *"%{http_code}"*) printf %s "$code"; exit 0 ;; esac
[ "$code" = 200 ]
S
printf '#!/bin/sh\nexit 0\n' > "$root/bin/cargo"
chmod +x "$root/bin/gh" "$root/bin/curl" "$root/bin/cargo"
PATH="$root/bin:$PATH"; export PATH

git init -q --bare -b main "$root/origin.git"
git clone -q "$root/origin.git" "$root/work" 2>/dev/null
cd "$root/work"
mkdir scripts
cp "$here/release.sh" scripts/release.sh
printf '#!/bin/sh\nexit 0\n' > scripts/terminal-context-check.sh
printf '[workspace.package]\nversion = "0.43.0"\n' > Cargo.toml
printf '## [0.43.0]\n' > CHANGELOG.md
git add -A; git commit -qm first
git push -q origin HEAD:refs/heads/main
git tag -a v0.43.0 -m old HEAD; git push -q origin refs/tags/v0.43.0
first=$(git rev-parse HEAD)
echo fix > fix.txt; git add -A; git commit -qm fix
second=$(git rev-parse HEAD)
remote_tag() { git --git-dir="$root/origin.git" rev-parse "refs/tags/v0.43.0^{commit}"; }
release() { env "$@" sh scripts/release.sh > "$root/out" 2>&1; }
check() { echo "ok: $1"; }
bad() { echo "FAIL: $1"; cat "$root/out"; exit 1; }

# 1 a published GitHub release or crates.io version refuses, and origin's tag stays
release STUB_RELEASE=1 && bad "a GitHub release did not refuse"
grep -q "GitHub has the release" "$root/out" || bad "no GitHub refusal text"
[ "$(remote_tag)" = "$first" ] || bad "tag moved over a GitHub release"
check "GitHub release refuses"
release STUB_CRATES=200 && bad "a crates.io version did not refuse"
grep -q "crates.io has wsx-core" "$root/out" || bad "no crates.io refusal text"
[ "$(remote_tag)" = "$first" ] || bad "tag moved over a crates.io version"
check "crates.io version refuses"
release STUB_CRATES=500 && bad "an unreadable crates.io did not refuse"
[ "$(remote_tag)" = "$first" ] || bad "tag moved on an unreadable crates.io"
check "unreadable crates.io refuses"

# 2 a dry run moves nothing
release WSX_RELEASE_DRY_RUN=1 || bad "dry run failed"
grep -q "dry run: git push --force-with-lease=refs/tags/v0.43.0:" "$root/out" || bad "dry run shows no lease push"
[ "$(remote_tag)" = "$first" ] || bad "dry run moved the tag"
check "dry run plans the lease push"

# 3 nothing published: the tag moves to the new commit (the local tag was on the old one)
release X=1 || bad "rerun failed"
[ "$(remote_tag)" = "$second" ] || bad "origin's tag is not on the new commit"
[ "$(git rev-parse v0.43.0^{commit})" = "$second" ] || bad "local tag is not on the new commit"
[ "$(git --git-dir="$root/origin.git" rev-parse refs/heads/main)" = "$second" ] || bad "main did not move"
check "tag moved to the new commit"

# 4 the tag already on the commit is not moved again
release X=1 && bad "a tag on the commit did not stop"
grep -q "already on this commit" "$root/out" || bad "no same-commit text"
check "same commit stops"

# 5 a lease that lost: origin's tag is not the value the preflight read
echo more > more.txt; git add -A; git commit -qm more
git tag -fa v0.43.0 -m third HEAD
if git push --force-with-lease="refs/tags/v0.43.0:$first" origin refs/tags/v0.43.0 >/dev/null 2>&1; then bad "a stale lease pushed"; fi
[ "$(remote_tag)" = "$second" ] || bad "the stale lease moved the tag"
check "a stale lease is refused by git"
echo "release rerun scenario passed"
