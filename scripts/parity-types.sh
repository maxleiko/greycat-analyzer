#!/usr/bin/env bash
# P18.2 — TS-vs-Rust typed-AST parity oracle (dump-types).
#
# Runs both the Rust port and the TypeScript reference's `dump-types`
# subcommand over the same project / file, sorts each output, and emits
# a unified diff.
#
# Exits non-zero when the diff is non-empty so the script composes into
# CI. The intentional-divergence allow-list at
# `tests/parity/divergences.toml` is subtracted from both outputs
# before comparing — the gate only fires on *unintended* drift.
#
# Usage:
#   scripts/parity-types.sh <project_or_file>
#
# Optional environment overrides:
#   GREYCAT_LANG     path to the TS reference binary (default: `greycat-lang`
#                    on PATH, then ~/.greycat/bin/greycat-lang)
#   GREYCAT_RUST     path to the Rust port binary (default: cargo run)
#   DIVERGENCES      path to the allow-list TOML (default:
#                    tests/parity/divergences.toml)
#
# Example:
#   scripts/parity-types.sh project.gcl
#   scripts/parity-types.sh ~/dev/datathings/greycat/apps/registry
set -euo pipefail

TARGET=${1:?"usage: $0 <project_or_file>"}

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)

# Locate the TS reference.
if [ -n "${GREYCAT_LANG:-}" ]; then
  TS_BIN="$GREYCAT_LANG"
elif command -v greycat-lang >/dev/null 2>&1; then
  TS_BIN=$(command -v greycat-lang)
elif [ -x "$HOME/.greycat/bin/greycat-lang" ]; then
  TS_BIN="$HOME/.greycat/bin/greycat-lang"
else
  echo "error: TS reference 'greycat-lang' not found. Set GREYCAT_LANG or add it to PATH." >&2
  exit 2
fi

# Locate the Rust port. Use a pre-built binary if requested, otherwise
# fall back to `cargo run -q`.
if [ -n "${GREYCAT_RUST:-}" ]; then
  RUST_RUN=("$GREYCAT_RUST")
else
  RUST_RUN=(cargo run --manifest-path "$REPO_ROOT/Cargo.toml" -q -p greycat-analyzer --)
fi

DIVERGENCES=${DIVERGENCES:-"$REPO_ROOT/tests/parity/divergences.toml"}

WORK=$(mktemp -d -t parity-types-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

TS_OUT="$WORK/ts.jsonl"
RUST_OUT="$WORK/rust.jsonl"

echo "[parity-types] running TS reference: $TS_BIN dump-types $TARGET"
"$TS_BIN" dump-types "$TARGET" > "$TS_OUT"

echo "[parity-types] running Rust port: ${RUST_RUN[*]} dump-types $TARGET"
"${RUST_RUN[@]}" dump-types "$TARGET" > "$RUST_OUT"

# Subtract intentional divergences from both outputs so the diff only
# surfaces unintended drift. The TOML format is documented at
# tests/parity/divergences.toml. The TS filter pass receives the
# Rust output via --rust-pre so position-widening entries can rewrite
# TS-side records that match a Rust shape at the same span.
filter_rust() {
  if [ ! -f "$DIVERGENCES" ]; then
    cat "$RUST_OUT"
    return
  fi
  python3 "$REPO_ROOT/scripts/_apply_divergences.py" "$DIVERGENCES" < "$RUST_OUT"
}

filter_ts() {
  if [ ! -f "$DIVERGENCES" ]; then
    cat "$TS_OUT"
    return
  fi
  python3 "$REPO_ROOT/scripts/_apply_divergences.py" "$DIVERGENCES" \
    --rust-pre "$RUST_OUT" \
    < "$TS_OUT"
}

TS_FILTERED="$WORK/ts.filtered.jsonl"
RUST_FILTERED="$WORK/rust.filtered.jsonl"
filter_ts | LC_ALL=C sort > "$TS_FILTERED"
filter_rust | LC_ALL=C sort > "$RUST_FILTERED"

echo "[parity-types] diff (TS → Rust), with allow-list applied:"
if diff -u "$TS_FILTERED" "$RUST_FILTERED"; then
  echo "[parity-types] OK — no diff"
  exit 0
else
  rust_lines=$(wc -l < "$RUST_FILTERED")
  ts_lines=$(wc -l < "$TS_FILTERED")
  echo "[parity-types] DIFF — rust=$rust_lines lines, ts=$ts_lines lines"
  exit 1
fi
