#!/usr/bin/env bash
# Stdlib parity check (P5.6).
#
# Reads the pinned stdlib version from `project.gcl` and verifies that
# `lib/std/` is populated and parses cleanly through the coverage
# gauntlet. Intended to be run locally and from CI after `greycat
# install`.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROJECT_GCL="$REPO_ROOT/project.gcl"
LIB_STD="$REPO_ROOT/lib/std"

if [ ! -f "$PROJECT_GCL" ]; then
    echo "missing $PROJECT_GCL" >&2
    exit 1
fi

PIN="$(awk -F'"' '/@library\("std"/ { print $4; exit }' "$PROJECT_GCL")"
if [ -z "$PIN" ]; then
    echo "no @library(\"std\", \"...\") pin found in $PROJECT_GCL" >&2
    exit 1
fi
echo "[stdlib] pin: $PIN"

if [ ! -d "$LIB_STD" ]; then
    echo "[stdlib] $LIB_STD missing — run \`greycat install\` from the repo root."
    echo "[stdlib] not blocking — the gauntlet skips the stdlib block when not installed."
    exit 0
fi

echo "[stdlib] running coverage gauntlet (includes lib/std/)..."
cargo test -p greycat-analyzer-syntax --test coverage --quiet
echo "[stdlib] OK"
