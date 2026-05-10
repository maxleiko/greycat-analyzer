#!/usr/bin/env bash
# P25.1: lint-throughput baseline. P27.2: single-binary mode.
#
# Builds `cargo build --release -p greycat-analyzer` and runs hyperfine
# on the resulting binary against each named corpus. Tracks lint
# throughput across phases. The P26 serial-vs-parallel A/B mode is
# gone — after P27 there's only one binary on native (parallel via
# the `crate::parallel` shim, no Cargo feature flag).
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

echo "[bench-lint] cargo build --release -p greycat-analyzer"
(cd "$REPO_ROOT" && cargo build --release -p greycat-analyzer >/dev/null)

BIN="$REPO_ROOT/target/release/greycat-analyzer"

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
        --command-name "$name" \
        "$BIN lint $path/project.gcl --format=compact > /dev/null"
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
