#!/usr/bin/env bash
# Playground-specific wrapper around the repo-root `scripts/build-wasm.sh`.
# Enables the `playground` cargo feature (CST / HIR / tokens / types /
# diagnostics / format dumpers) and emits into the playground's
# expected location at `greycat-analyzer-wasm/pkg/`.
#
# The published `@greycat/analyzer` npm package uses a separate
# invocation (see `packages/analyzer/package.json`'s `wasm` script) that
# omits `--features playground` so the shipped bundle stays small.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)/.."
exec "$REPO_ROOT/scripts/build-wasm.sh" \
    --out-dir greycat-analyzer-wasm/pkg \
    --features playground \
    "$@"
