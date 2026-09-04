#!/bin/sh
set -eu

root=${1:-.}
if [ ! -d "$root" ]; then
  printf 'detect-flow: project root is not a directory: %s\n' "$root" >&2
  exit 64
fi
cd "$root"

workflows=.github/workflows
has_workflows=false
[ -d "$workflows" ] && has_workflows=true

contains_workflow() {
  [ "$has_workflows" = true ] && grep -RIlE "$1" "$workflows" >/dev/null 2>&1
}

has_korean=false
if [ -f CHANGELOG.md ] && grep -Eq '새로운|버그|기능' CHANGELOG.md; then
  has_korean=true
fi

if [ -f Cargo.toml ]; then
  if contains_workflow 'cargo[[:space:]]+publish|gh[[:space:]]+release|action-gh-release|release-action|homebrew' && \
     contains_workflow 'refs/tags/|tags:[[:space:]]*|github\.ref.*tag|release:'; then
    printf '%s\n' 'rust-ci'
    printf '%s\n' 'signal: Cargo.toml and tag/release-driven Rust publish workflow' >&2
  else
    printf '%s\n' 'rust'
    printf '%s\n' 'signal: Cargo.toml without detected tag/release publish workflow' >&2
  fi
  exit 0
fi

# Require both a tag-oriented workflow and container image publication for GitOps.
if contains_workflow 'docker/(build-push-action|login-action)|docker[[:space:]]+(build|push)|buildah|kaniko' && \
   contains_workflow 'tags:[[:space:]]*|refs/tags/|github\.ref.*tag|newTag|argocd'; then
  if [ "$has_korean" = true ]; then
    printf '%s\n' 'gh-ko-gitops'
  else
    printf '%s\n' 'gh-en-gitops'
  fi
  printf '%s\n' 'signal: tag-oriented container publish/deploy workflow' >&2
  exit 0
fi

if [ -f package.json ]; then
  if contains_workflow 'npm[[:space:]]+publish|pnpm[[:space:]]+publish|yarn[[:space:]]+npm[[:space:]]+publish'; then
    printf '%s\n' 'node-ci'
    printf '%s\n' 'signal: package.json and Node publish workflow' >&2
  elif grep -Eq '"version"[[:space:]]*:' package.json; then
    printf '%s\n' 'node'
    printf '%s\n' 'signal: versioned package.json without detected publish workflow' >&2
  else
    printf 'detect-flow: package.json has no version and no publish workflow; classification is ambiguous\n' >&2
    exit 65
  fi
  exit 0
fi

if [ "$has_korean" = true ]; then
  printf '%s\n' 'gh-ko'
  printf '%s\n' 'signal: Korean changelog and no package flow' >&2
else
  printf '%s\n' 'gh-en'
  printf '%s\n' 'signal: default GitHub release flow' >&2
fi
