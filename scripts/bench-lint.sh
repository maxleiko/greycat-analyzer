#!/usr/bin/env bash
# P25.1: lint-throughput baseline. P26.1: serial-vs-parallel mode.
#
# Builds two release binaries — one with `--features parallel`, one
# without — into separate `target/par-{off,on}` dirs, then runs hyperfine
# across both for each named corpus, side-by-side. The hyperfine summary
# table reads off the speedup directly.
#
# Until the `parallel` feature exists (P26.2 introduces it), the parallel
# build falls back to the serial code path — both rows show the same
# number, which is the right behaviour for chunks that haven't enabled
# rayon yet.
#
# Corpora are referenced by short name only ("pro", "solarleb"). Their
# absolute paths live in the env (BENCH_PRO / BENCH_SOLARLEB) so the
# script stays public-shareable without leaking implementation paths.
#
# Usage:
#   scripts/bench-lint.sh                          # run every configured corpus
#   scripts/bench-lint.sh pro solarleb             # explicit subset
#   BENCH_PRO=/path/to/pro scripts/bench-lint.sh   # override env path
#
# Defaults to the paths the maintainer uses locally; override via env.
set -euo pipefail

: "${BENCH_PRO:=/home/leiko/dev/datathings/greycat/pro}"
: "${BENCH_SOLARLEB:=/home/leiko/dev/datathings/assaad/solarleb}"
: "${BENCH_WARMUP:=3}"
: "${BENCH_RUNS:=20}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Build both flavours into separate target dirs so the binaries can
# coexist on disk and hyperfine can A/B them without rebuilds in
# between.
echo "[bench-lint] cargo build --release -p greycat-analyzer (serial)"
(cd "$REPO_ROOT" && CARGO_TARGET_DIR=target/par-off \
    cargo build --release -p greycat-analyzer >/dev/null)

echo "[bench-lint] cargo build --release -p greycat-analyzer --features parallel"
(cd "$REPO_ROOT" && CARGO_TARGET_DIR=target/par-on \
    cargo build --release -p greycat-analyzer --features parallel >/dev/null)

BIN_OFF="$REPO_ROOT/target/par-off/release/greycat-analyzer"
BIN_ON="$REPO_ROOT/target/par-on/release/greycat-analyzer"

run_corpus() {
    local name="$1" path="$2"
    if [[ ! -d "$path" ]]; then
        echo "[bench-lint] skip $name: $path not found" >&2
        return
    fi
    if [[ ! -f "$path/project.gcl" ]]; then
        echo "[bench-lint] skip $name: $path/project.gcl missing" >&2
        return
    fi
    echo
    echo "=== $name ==="
    # `lint` exits non-zero whenever any diagnostic is emitted (warnings
    # included), which hyperfine treats as a benchmark failure unless we
    # opt out — we just want the wall-clock cost of analysis.
    hyperfine --warmup "$BENCH_WARMUP" --runs "$BENCH_RUNS" --ignore-failure \
        --command-name "serial   ($name)" \
        "$BIN_OFF lint $path/project.gcl --format=compact > /dev/null" \
        --command-name "parallel ($name)" \
        "$BIN_ON  lint $path/project.gcl --format=compact > /dev/null"
}

targets=("$@")
if [[ ${#targets[@]} -eq 0 ]]; then
    targets=(pro solarleb)
fi

for name in "${targets[@]}"; do
    case "$name" in
        pro)      run_corpus pro      "$BENCH_PRO" ;;
        solarleb) run_corpus solarleb "$BENCH_SOLARLEB" ;;
        *) echo "[bench-lint] unknown corpus: $name" >&2; exit 2 ;;
    esac
done
