#!/usr/bin/env bash
# P10.1 — crates.io publish driver.
#
# Publishes every crate in dependency order. Run with `--dry-run`
# first; this script does NOT pass `--dry-run` automatically because
# real publishing is destructive and irreversible.
#
# Pre-flight (do these before running):
#   1. `cargo login <token>` is configured.
#   2. The submodule `vendor/tree-sitter-greycat` is published as a
#      crate (or its parser.c vendored into greycat-analyzer-syntax)
#      — `greycat-analyzer-syntax` currently uses a path dep to it
#      and won't publish without that.
#   3. The CI gauntlet (build / clippy / test) is green.
#   4. The repo is on a clean tag (e.g. `v0.1.0`) — Cargo's package
#      registry rejects re-publishing the same `version`.
set -euo pipefail

ORDER=(
  greycat-analyzer-syntax
  greycat-analyzer-core
  greycat-analyzer-hir
  greycat-analyzer-types
  greycat-analyzer-fmt
  greycat-analyzer-analysis
  greycat-analyzer-ls
  greycat-analyzer-wasm
  greycat-analyzer
)

DRY="${1:-}"

for crate in "${ORDER[@]}"; do
  echo "=== publishing $crate ==="
  if [[ "$DRY" == "--dry-run" ]]; then
    cargo publish --dry-run -p "$crate"
  else
    cargo publish -p "$crate"
  fi
  # Wait for the crates.io registry to settle so the next crate's
  # dep on this one resolves.
  if [[ "$DRY" != "--dry-run" ]]; then
    sleep 30
  fi
done
