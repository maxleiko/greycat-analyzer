#!/usr/bin/env bash
# P10.3: TS-vs-Rust diagnostic parity oracle (harness only).
#
# Runs the Rust port and the TypeScript reference (if available) over
# the same corpus of `.gcl` files, captures their CLI diagnostic
# output, normalizes both into a comparable shape, and emits a diff.
#
# This is the *harness*; the parity scorer / CI gate that closes
# ROADMAP §7-A lives behind P6 fully landing first (so the diff isn't
# overwhelming). For now, the script is a way to take a snapshot
# pre-/post-change and eyeball the diff for regressions.
#
# Usage:
#   scripts/parity-oracle.sh <path-to-ts-lang-checkout> <corpus-dir>
#
# Example:
#   scripts/parity-oracle.sh /path/to/datathings/greycat/lang lib/std
set -euo pipefail

TS_ROOT=${1:?"usage: $0 <ts-lang-checkout> <corpus-dir>"}
CORPUS=${2:?"usage: $0 <ts-lang-checkout> <corpus-dir>"}
WORK=$(mktemp -d -t parity-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

RUST_OUT="$WORK/rust.txt"
TS_OUT="$WORK/ts.txt"

echo "[parity] running Rust analyzer over $CORPUS"
cargo run -q -p greycat-analyzer --release -- lint "$CORPUS"/$(ls "$CORPUS" | head -1) > "$RUST_OUT" 2>&1 || true

if [ -d "$TS_ROOT/packages/cli" ]; then
  echo "[parity] running TS reference over $CORPUS"
  (cd "$TS_ROOT" && pnpm --filter @greycat/cli lint "$CORPUS") > "$TS_OUT" 2>&1 || true
else
  echo "[parity] TS reference not found at $TS_ROOT — capturing Rust output only" >&2
  : > "$TS_OUT"
fi

# Normalize both outputs: strip absolute paths, drop timing summaries,
# sort by path:line:col so order differences don't dominate the diff.
normalize() {
  sed -E 's|^[^:]*/||' "$1" \
    | grep -E '^[^[:space:]]+\.gcl:[0-9]+:[0-9]+:' \
    | sort
}

RUST_NORM="$WORK/rust.norm"
TS_NORM="$WORK/ts.norm"
normalize "$RUST_OUT" > "$RUST_NORM"
normalize "$TS_OUT"   > "$TS_NORM"

echo "[parity] diff (TS → Rust):"
diff -u "$TS_NORM" "$RUST_NORM" || true

# Expose the totals so CI can wire this into a regression budget once
# the diff is small enough to be useful.
echo "[parity] rust diagnostics: $(wc -l < "$RUST_NORM")"
echo "[parity] ts   diagnostics: $(wc -l < "$TS_NORM")"
