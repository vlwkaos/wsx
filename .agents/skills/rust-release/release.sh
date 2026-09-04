#!/usr/bin/env bash
set -euo pipefail

# Build and publish a macOS universal Rust CLI release.
# Run from the Rust project's repository root.
#
# Usage:
#   release.sh <version> <binary-name> <github-owner/repo> [tap-path]
#   release.sh --version <version> --binary <name> --repo <owner/repo> \
#       [--tap-path <path> | --tap <owner/homebrew-tap>]
#
# The named inputs may also be supplied as RUST_RELEASE_VERSION,
# RUST_RELEASE_BINARY, RUST_RELEASE_REPO, RUST_RELEASE_TAP_PATH, and
# RUST_RELEASE_TAP. A tap path takes precedence over a tap name.

usage() {
    cat <<'EOF'
Usage:
  release.sh <version> <binary-name> <github-owner/repo> [tap-path]
  release.sh --version <version> --binary <name> --repo <owner/repo> \
      [--tap-path <path> | --tap <owner/homebrew-tap>]

Creates an optionally signed universal macOS tarball and Homebrew bottle, tags
and pushes main, creates a GitHub release, optionally publishes to crates.io, runs project
release extras, and updates a configured Homebrew tap for stable versions.

Configuration environment variables:
  RUST_RELEASE_VERSION, RUST_RELEASE_BINARY, RUST_RELEASE_REPO,
  RUST_RELEASE_TAP_PATH, RUST_RELEASE_TAP
EOF
}

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

VERSION="${RUST_RELEASE_VERSION:-}"
BINARY_NAME="${RUST_RELEASE_BINARY:-}"
GITHUB_REPO="${RUST_RELEASE_REPO:-}"
TAP_PATH="${RUST_RELEASE_TAP_PATH:-}"
TAP_NAME="${RUST_RELEASE_TAP:-}"

if [[ $# -gt 0 && "$1" != --* ]]; then
    [[ $# -ge 3 && $# -le 4 ]] || { usage >&2; exit 2; }
    VERSION="$1"
    BINARY_NAME="$2"
    GITHUB_REPO="$3"
    TAP_PATH="${4:-$TAP_PATH}"
else
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --version) [[ $# -ge 2 ]] || die "--version requires a value"; VERSION="$2"; shift 2 ;;
            --binary) [[ $# -ge 2 ]] || die "--binary requires a value"; BINARY_NAME="$2"; shift 2 ;;
            --repo) [[ $# -ge 2 ]] || die "--repo requires a value"; GITHUB_REPO="$2"; shift 2 ;;
            --tap-path) [[ $# -ge 2 ]] || die "--tap-path requires a value"; TAP_PATH="$2"; shift 2 ;;
            --tap) [[ $# -ge 2 ]] || die "--tap requires a value"; TAP_NAME="$2"; shift 2 ;;
            --help|-h) usage; exit 0 ;;
            *) die "unknown option: $1" ;;
        esac
    done
fi

[[ -n "$VERSION" ]] || die "version is required"
[[ -n "$BINARY_NAME" ]] || die "binary name is required"
[[ -n "$GITHUB_REPO" ]] || die "GitHub repository is required"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] || \
    die "version must be a semantic version such as 1.2.3 or 1.2.3-rc.1"
[[ "$BINARY_NAME" =~ ^[A-Za-z0-9][A-Za-z0-9_-]*$ ]] || \
    die "binary name may contain only letters, numbers, underscores, and hyphens"
[[ "$GITHUB_REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || \
    die "GitHub repository must be owner/repository"
[[ -z "$TAP_PATH" || -z "$TAP_NAME" ]] || die "use either a tap path or a tap name, not both"

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required"
}

# --- Validate ---
for command_name in gh gpg cargo rustup git lipo otool tar shasum; do
    require_command "$command_name"
done
gh auth status --hostname github.com >/dev/null || die "GitHub CLI is not authenticated for github.com"
test -f Cargo.toml || die "Cargo.toml not found; run this script from the Rust project root"
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || die "not inside a Git repository"
[[ "$(git symbolic-ref --quiet --short HEAD)" == "main" ]] || die "release must run from the main branch"
git remote get-url origin >/dev/null 2>&1 || die "Git remote 'origin' is required"
git fetch --quiet origin main --tags || die "failed to fetch origin/main and tags"
git merge-base --is-ancestor refs/remotes/origin/main HEAD || \
    die "local main is behind or diverged from origin/main"
git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null && die "tag v$VERSION already exists"
if gh release view "v$VERSION" --repo "$GITHUB_REPO" >/dev/null 2>&1; then
    die "GitHub release v$VERSION already exists in $GITHUB_REPO"
fi

# A release tag captures only committed source. Generated release files should
# already be ignored; any remaining work must be deliberately committed first.
if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
    die "working tree is not clean; commit or remove all source changes before releasing"
fi

# --- Ensure .gitignore has security patterns ---
SECURITY_PATTERNS=(
    '*.pem' '*.key' '*.p12' '*.pfx' '*.asc' '*.tar.gz' '*.bottle.tar.gz'
    '*.sha256' '.env*' '*secret*' '*credential*'
)
GITIGNORE_UPDATED=false
for pattern in "${SECURITY_PATTERNS[@]}"; do
    if ! grep -qxF "$pattern" .gitignore 2>/dev/null; then
        printf '%s\n' "$pattern" >> .gitignore
        GITIGNORE_UPDATED=true
    fi
done
if [[ "$GITIGNORE_UPDATED" == true ]]; then
    git add .gitignore
    git commit -m "chore: add security patterns to .gitignore"
    printf 'Updated .gitignore with security patterns\n'
fi

# --- Security audit ---
SENSITIVE=$(git ls-files | grep -E '\.(pem|key|p12|pfx)$|\.env|secret|credential' || true)
if [[ -n "$SENSITIVE" ]]; then
    printf 'ABORT: Sensitive files detected:\n%s\n' "$SENSITIVE" >&2
    printf 'Run: git rm --cached <file>\n' >&2
    exit 1
fi

# --- Update version ---
MANIFEST_TMP=$(mktemp)
if ! awk -v new_version="$VERSION" '
    /^\[package\][[:space:]]*$/ { in_package=1; print; next }
    /^\[/ { in_package=0 }
    in_package && !changed && /^version[[:space:]]*=[[:space:]]*"[^"]*"[[:space:]]*$/ {
        print "version = \"" new_version "\""
        changed=1
        next
    }
    { print }
    END { if (!changed) exit 3 }
' Cargo.toml > "$MANIFEST_TMP"; then
    rm -f "$MANIFEST_TMP"
    die "Cargo.toml has no root [package] version to update"
fi
cat "$MANIFEST_TMP" > Cargo.toml
rm -f "$MANIFEST_TMP"
printf 'Updated Cargo.toml to v%s\n' "$VERSION"

# --- Build universal binary (both targets required) ---
ARM_TARGET='aarch64-apple-darwin'
X86_TARGET='x86_64-apple-darwin'
for target in "$ARM_TARGET" "$X86_TARGET"; do
    rustup target list --installed | grep -qx "$target" || \
        die "required target '$target' is not installed; run: rustup target add $target"
done

ARCH_LABEL='darwin-universal'
printf 'Building universal binary (arm64 minos=11.0, x86_64 minos=10.15)...\n'
MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release --target "$ARM_TARGET" --bin "$BINARY_NAME"
MACOSX_DEPLOYMENT_TARGET=10.15 cargo build --release --target "$X86_TARGET" --bin "$BINARY_NAME"
mkdir -p target/release-dist
RELEASE_BINARY="target/release-dist/$BINARY_NAME"
lipo -create -output "$RELEASE_BINARY" \
    "target/$ARM_TARGET/release/$BINARY_NAME" \
    "target/$X86_TARGET/release/$BINARY_NAME"
printf 'Universal binary created via lipo\n'

test -x "$RELEASE_BINARY" || die "binary not found or not executable: $RELEASE_BINARY"
printf 'Built: %s  minos=%s\n' "$ARCH_LABEL" "$(otool -l "$RELEASE_BINARY" | awk '/minos/{print $2; exit}')"

TARBALL="${BINARY_NAME}-${VERSION}-${ARCH_LABEL}.tar.gz"
BOTTLE_NAME="${BINARY_NAME}-${VERSION}.all.bottle.tar.gz"
NOTES_FILE=''
BOTTLE_DIR=''
cleanup() {
    [[ -z "$NOTES_FILE" ]] || rm -f "$NOTES_FILE"
    [[ -z "$BOTTLE_DIR" ]] || rm -rf "$BOTTLE_DIR"
}
trap cleanup EXIT

# --- Package ---
tar -czvf "$TARBALL" -C "$(dirname "$RELEASE_BINARY")" "$BINARY_NAME"
SHA256=$(shasum -a 256 "$TARBALL" | awk '{print $1}')
printf '%s  %s\n' "$SHA256" "$TARBALL" > "$TARBALL.sha256"
if gpg --batch --list-secret-keys 2>/dev/null | grep -q '^sec'; then
    gpg --batch --yes --armor --detach-sign "$TARBALL"
else
    printf 'GPG key not available, skipping signing\n'
fi

# --- Bottle ---
# Structure: binary_name/version/bin/binary_name (cellar: :any_skip_relocation)
BOTTLE_DIR=$(mktemp -d)
mkdir -p "$BOTTLE_DIR/${BINARY_NAME}/${VERSION}/bin"
cp "$RELEASE_BINARY" "$BOTTLE_DIR/${BINARY_NAME}/${VERSION}/bin/${BINARY_NAME}"
tar -czf "$BOTTLE_NAME" -C "$BOTTLE_DIR" "$BINARY_NAME"
rm -rf "$BOTTLE_DIR"
BOTTLE_DIR=''
BOTTLE_SHA256=$(shasum -a 256 "$BOTTLE_NAME" | awk '{print $1}')
printf 'Bottle: %s (%s)\n' "$BOTTLE_NAME" "$BOTTLE_SHA256"

# --- Git ---
GIT_RELEASE_FILES=(Cargo.toml)
[[ -f Cargo.lock ]] && GIT_RELEASE_FILES+=(Cargo.lock)
git add "${GIT_RELEASE_FILES[@]}"
git diff --cached --quiet || git commit -m "v$VERSION"
git tag -a "v$VERSION" -m "v$VERSION"
git push origin main "refs/tags/v$VERSION"

# --- GitHub release ---
RELEASE_FILES=("$TARBALL" "$TARBALL.sha256" "$BOTTLE_NAME")
[[ -f "$TARBALL.asc" ]] && RELEASE_FILES+=("$TARBALL.asc")
NOTES_FILE=$(mktemp)
if [[ -f CHANGELOG.md ]]; then
    awk -v heading="## [$VERSION]" '
        index($0, heading) == 1 { found=1; next }
        found && /^## \[/ { exit }
        found { print }
    ' CHANGELOG.md > "$NOTES_FILE"
fi
PRERELEASE_ARGS=()
[[ "$VERSION" == *-* ]] && PRERELEASE_ARGS+=(--prerelease)
if [[ -s "$NOTES_FILE" ]]; then
    gh release create "v$VERSION" --repo "$GITHUB_REPO" --title "v$VERSION" \
        ${PRERELEASE_ARGS[@]+"${PRERELEASE_ARGS[@]}"} --notes-file "$NOTES_FILE" "${RELEASE_FILES[@]}"
else
    gh release create "v$VERSION" --repo "$GITHUB_REPO" --title "v$VERSION" \
        ${PRERELEASE_ARGS[@]+"${PRERELEASE_ARGS[@]}"} --generate-notes "${RELEASE_FILES[@]}"
fi
rm -f "$NOTES_FILE"
NOTES_FILE=''

RELEASE_ASSETS=$(gh release view "v$VERSION" --repo "$GITHUB_REPO" --json assets --jq '.assets[].name')
printf '%s\n' "$RELEASE_ASSETS" | grep -Fx "$TARBALL" >/dev/null || \
    die "release is missing expected tarball asset: $TARBALL"
printf '%s\n' "$RELEASE_ASSETS" | grep -Fx "$BOTTLE_NAME" >/dev/null || \
    die "release is missing expected bottle asset: $BOTTLE_NAME"

# --- crates.io ---
if grep -q '^\[package\]' Cargo.toml && ! grep -q '^publish[[:space:]]*=[[:space:]]*false' Cargo.toml; then
    cargo publish
fi

# --- Project-specific extras ---
for extras in ./scripts/*release*.sh; do
    [[ -f "$extras" ]] || continue
    printf 'Running project release script: %s\n' "$extras"
    export VERSION BINARY_NAME GITHUB_REPO ARCH_LABEL
    bash "$extras"
done

# --- Homebrew (skip prerelease) ---
if [[ "$VERSION" == *-* ]]; then
    printf 'Prerelease detected, skipping Homebrew update\n'
    exit 0
fi
if [[ -n "$TAP_NAME" ]]; then
    require_command brew
    TAP_PATH=$(brew --repository "$TAP_NAME") || \
        die "Homebrew tap '$TAP_NAME' is unavailable; install it or provide --tap-path"
fi
if [[ -z "$TAP_PATH" ]]; then
    printf 'Homebrew tap is not configured; skipping update (set --tap-path, --tap, RUST_RELEASE_TAP_PATH, or RUST_RELEASE_TAP)\n'
    exit 0
fi
if [[ ! -d "$TAP_PATH" ]]; then
    printf 'Tap not found: %s; skipping Homebrew update\n' "$TAP_PATH"
    exit 0
fi
git -C "$TAP_PATH" rev-parse --is-inside-work-tree >/dev/null 2>&1 || \
    die "Homebrew tap path is not a Git repository: $TAP_PATH"
if [[ -n "$(git -C "$TAP_PATH" status --porcelain --untracked-files=all)" ]]; then
    die "Homebrew tap working tree is not clean: $TAP_PATH"
fi

DESC=$(grep '^description[[:space:]]*=' Cargo.toml | head -1 | sed -E 's/^description[[:space:]]*=[[:space:]]*"(.*)"$/\1/')
DESC=${DESC:-"$BINARY_NAME"}
DESC_ESCAPED=$(printf '%s' "$DESC" | sed 's/\\/\\\\/g; s/"/\\"/g')
CLASS_NAME=$(printf '%s' "$BINARY_NAME" | sed 's/-/ /g' | \
    awk '{for(i=1;i<=NF;i++) $i=toupper(substr($i,1,1)) tolower(substr($i,2))}1' | tr -d ' ')
FORMULA_REL="Formula/${BINARY_NAME}.rb"
FORMULA_PATH="$TAP_PATH/$FORMULA_REL"
mkdir -p "$TAP_PATH/Formula"
cat > "$FORMULA_PATH" <<EOF
class ${CLASS_NAME} < Formula
  desc "${DESC_ESCAPED}"
  homepage "https://github.com/${GITHUB_REPO}"
  url "https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/${TARBALL}"
  sha256 "${SHA256}"
  license "MIT"

  bottle do
    root_url "https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}"
    sha256 cellar: :any_skip_relocation, all: "${BOTTLE_SHA256}"
  end

  def install
    bin.install "${BINARY_NAME}"
  end

  test do
    assert_predicate bin/"${BINARY_NAME}", :executable?
  end
end
EOF

grep -F "url \"https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/${TARBALL}\"" "$FORMULA_PATH" >/dev/null || \
    die 'generated formula tarball URL does not match expected release asset'
grep -F "sha256 cellar: :any_skip_relocation, all: \"${BOTTLE_SHA256}\"" "$FORMULA_PATH" >/dev/null || \
    die 'generated formula bottle SHA does not match packaged bottle'

(
    cd "$TAP_PATH"
    git add -- "$FORMULA_REL"
    git diff --cached --quiet || git commit -m "$BINARY_NAME $VERSION"
    git push
)

printf 'Released %s v%s\n' "$BINARY_NAME" "$VERSION"
printf 'RELEASE_TAG=v%s\n' "$VERSION"
