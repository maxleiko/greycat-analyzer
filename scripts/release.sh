#!/usr/bin/env bash
# Release driver — bump the workspace version, tag, and push.
#
# Pushing the `v*` tag triggers .github/workflows/release.yml, which
# builds the per-platform binaries and drafts a GitHub Release. The
# release is created in DRAFT mode; promote it to published from the
# GitHub UI once CI is green.
#
# Usage:
#   scripts/release.sh <version>      # e.g. scripts/release.sh 0.1.19
#   scripts/release.sh -y <version>   # skip the pre-push confirmation
#
# <version> has NO `v` prefix here; the prefix is added at tag time.
# Pre-release suffixes (0.2.0-rc1) are accepted.
set -euo pipefail

# Run from the repo root regardless of where the script is invoked.
cd "$(dirname "$0")/.."

YES=0
if [[ "${1:-}" == "-y" || "${1:-}" == "--yes" ]]; then
  YES=1
  shift
fi

VERSION="${1:-}"
CURRENT="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

if [[ -z "$VERSION" ]]; then
  echo "current version: $CURRENT"
  echo "usage: scripts/release.sh [-y] <version>   (no 'v' prefix)"
  exit 1
fi

if [[ "$VERSION" == v* ]]; then
  echo "error: drop the 'v' prefix -- pass '${VERSION#v}', the tag adds it." >&2
  exit 1
fi

TAG="v$VERSION"

echo "=== pre-flight ==="
# 1. Clean working tree.
if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: working tree is dirty -- commit/stash/discard first." >&2
  exit 1
fi
# 2. On main.
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$BRANCH" != "main" ]]; then
  echo "error: on '$BRANCH', not 'main'. Releases come from main." >&2
  exit 1
fi
# 3. Up to date with origin/main.
git fetch -q origin main
if [[ "$(git rev-list HEAD..origin/main --count)" != "0" ]]; then
  echo "error: local main is behind origin/main -- pull/rebase first." >&2
  exit 1
fi
# 4. Tag must not already exist (locally or on the remote).
if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "error: tag $TAG already exists locally." >&2
  exit 1
fi
if git ls-remote --exit-code --tags origin "$TAG" >/dev/null 2>&1; then
  echo "error: tag $TAG already exists on origin." >&2
  exit 1
fi
echo "ok: clean tree on main, up to date, $TAG free ($CURRENT -> $VERSION)"

echo "=== gauntlet ==="
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

echo "=== bump ==="
# Only the [workspace.package] version line matches `^version = "..."`;
# every crate inherits via `version.workspace = true`.
sed -i -E "s/^version = \"[^\"]*\"/version = \"$VERSION\"/" Cargo.toml
if ! grep -q "^version = \"$VERSION\"$" Cargo.toml; then
  echo "error: version bump did not apply to Cargo.toml." >&2
  exit 1
fi
# Refresh Cargo.lock with the new crate versions.
cargo build --workspace >/dev/null 2>&1
git --no-pager diff Cargo.toml Cargo.lock

if [[ "$YES" != "1" ]]; then
  read -r -p "Commit, tag $TAG, and push to origin? [y/N] " reply
  if [[ "$reply" != "y" && "$reply" != "Y" ]]; then
    echo "aborted -- reverting Cargo.toml/Cargo.lock."
    git checkout -- Cargo.toml Cargo.lock
    exit 1
  fi
fi

echo "=== commit, tag, push ==="
git add Cargo.toml Cargo.lock
git commit -m "chore(release): $TAG"
git tag -a "$TAG" -m "$TAG"
git push origin main
git push origin "$TAG"

echo
echo "released $TAG -- the tag push triggers the release workflow."
echo "  actions: https://github.com/maxleiko/greycat-analyzer/actions/workflows/release.yml"
echo "  drafts:  https://github.com/maxleiko/greycat-analyzer/releases"
echo "Review the draft artifacts, then promote it to published from the GitHub UI."
