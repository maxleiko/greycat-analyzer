#!/usr/bin/env bash
# P18.2 — TS-vs-Rust resolver parity oracle (dump-resolutions).
#
# Same shape as parity-types.sh but for the `dump-resolutions`
# subcommand. See that script for a full usage description.
set -euo pipefail

TARGET=${1:?"usage: $0 <project_or_file>"}

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)

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

if [ -n "${GREYCAT_RUST:-}" ]; then
  RUST_RUN=("$GREYCAT_RUST")
else
  RUST_RUN=(cargo run --manifest-path "$REPO_ROOT/Cargo.toml" -q -p greycat-analyzer --)
fi

DIVERGENCES=${DIVERGENCES:-"$REPO_ROOT/tests/parity/divergences.toml"}

WORK=$(mktemp -d -t parity-res-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

TS_OUT="$WORK/ts.jsonl"
RUST_OUT="$WORK/rust.jsonl"

echo "[parity-resolutions] running TS reference: $TS_BIN dump-resolutions $TARGET"
"$TS_BIN" dump-resolutions "$TARGET" > "$TS_OUT"

echo "[parity-resolutions] running Rust port: ${RUST_RUN[*]} dump-resolutions $TARGET"
"${RUST_RUN[@]}" dump-resolutions "$TARGET" > "$RUST_OUT"

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

echo "[parity-resolutions] diff (TS → Rust), with allow-list applied:"
if diff -u "$TS_FILTERED" "$RUST_FILTERED"; then
  echo "[parity-resolutions] OK — no diff"
  exit 0
else
  rust_lines=$(wc -l < "$RUST_FILTERED")
  ts_lines=$(wc -l < "$TS_FILTERED")
  echo "[parity-resolutions] DIFF — rust=$rust_lines lines, ts=$ts_lines lines"
  exit 1
fi
