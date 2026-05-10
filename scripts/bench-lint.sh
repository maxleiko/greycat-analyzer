#!/usr/bin/env bash
# P25.1: lint-throughput baseline.
#
# Runs `cargo run --release -p greycat-analyzer -- lint <project>` under
# hyperfine for each named corpus, so the SmolStr / FxHashMap / SmallVec
# chunks (P25.2-P25.8) can quote a delta against a recorded reference.
#
# When `BENCH_REF` points at the TypeScript reference's `greycat-lang`
# binary (defaults to `$HOME/.greycat/bin/greycat-lang` if it exists),
# each corpus is benched against both the Rust port and the reference,
# rendered side-by-side in a single hyperfine table so the speedup is
# read directly off the output.
#
# Corpora are referenced by short name only ("pro", "solarleb"). Their
# absolute paths live in the env (BENCH_PRO / BENCH_SOLARLEB) so the
# script stays public-shareable without leaking implementation paths.
#
# Usage:
#   scripts/bench-lint.sh                          # run every configured corpus
#   scripts/bench-lint.sh pro solarleb             # explicit subset
#   BENCH_PRO=/path/to/pro scripts/bench-lint.sh   # override env path
#   BENCH_REF= scripts/bench-lint.sh               # disable reference comparison
#
# Defaults to the paths the maintainer uses locally; override via env.
set -euo pipefail

: "${BENCH_PRO:=/home/leiko/dev/datathings/greycat/pro}"
: "${BENCH_SOLARLEB:=/home/leiko/dev/datathings/assaad/solarleb}"
: "${BENCH_WARMUP:=3}"
: "${BENCH_RUNS:=20}"
: "${BENCH_REF:=$HOME/.greycat/bin/greycat-lang}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Build once in release so the timed runs don't include the build.
echo "[bench-lint] cargo build --release -p greycat-analyzer"
(cd "$REPO_ROOT" && cargo build --release -p greycat-analyzer >/dev/null)

BIN="$REPO_ROOT/target/release/greycat-analyzer"

# When the reference impl exists, hyperfine runs both side-by-side
# (single warmup + comparison table). Otherwise fall back to a Rust-only
# bench so the script stays useful in environments without it.
have_ref=0
if [[ -n "$BENCH_REF" && -x "$BENCH_REF" ]]; then
    have_ref=1
fi

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
    if (( have_ref == 1 )); then
        hyperfine --warmup "$BENCH_WARMUP" --runs "$BENCH_RUNS" --ignore-failure \
            --command-name "rust ($name)" \
            "$BIN lint $path/project.gcl --format=compact > /dev/null" \
            --command-name "ref  ($name)" \
            "$BENCH_REF lint $path/project.gcl > /dev/null"
    else
        hyperfine --warmup "$BENCH_WARMUP" --runs "$BENCH_RUNS" --ignore-failure \
            --command-name "rust ($name)" \
            "$BIN lint $path/project.gcl --format=compact > /dev/null"
    fi
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
