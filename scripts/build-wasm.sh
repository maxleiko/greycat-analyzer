#!/usr/bin/env bash
# Build greycat-analyzer-wasm with wasm-pack and emit the JS bindings into
# a caller-specified directory.
#
# Two consumer modes:
#
#   --out-dir packages/analyzer/wasm           # @greycat/analyzer npm pkg
#   --out-dir greycat-analyzer-wasm/pkg \      # playground (debug dumpers)
#       --features playground
#
# Both modes emit `--target bundler` so the produced JS module imports
# the sibling `.wasm` file via the bundler's wasm support
# (`rolldown-plugin-wasm` for tsdown; Vite handles it natively for the
# playground app).
#
# wasm-pack drives `cargo build --target wasm32-unknown-unknown`, which
# builds tree-sitter-greycat's parser.c via clang. Clang for
# wasm32-unknown-unknown has no libc by default, so point it at the
# Emscripten sysroot for the missing headers (`stdlib.h`, etc.).
#
# Honors $EMSDK if set; otherwise falls back to the default
# `~/app/emsdk` path used in the maintainer's environment. Override
# with `EMSDK=/path/to/emsdk ./scripts/build-wasm.sh ...` if needed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WASM_CRATE="$REPO_ROOT/greycat-analyzer-wasm"

EMSDK_ROOT="${EMSDK:-${EMSDK_ROOT:-$HOME/app/emsdk}}"
SYSROOT="$EMSDK_ROOT/upstream/emscripten/cache/sysroot"

if [ ! -d "$SYSROOT" ]; then
    echo "error: missing emscripten sysroot at $SYSROOT" >&2
    echo "       set EMSDK=/path/to/emsdk and re-run" >&2
    exit 1
fi

OUT_DIR=""
FEATURE_ARGS=()
EXTRA_ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --out-dir)
            shift
            OUT_DIR="$1"
            shift
            ;;
        --features)
            shift
            FEATURE_ARGS+=("--features" "$1")
            shift
            ;;
        *)
            EXTRA_ARGS+=("$1")
            shift
            ;;
    esac
done

if [ -z "$OUT_DIR" ]; then
    echo "error: --out-dir is required (path relative to repo root)" >&2
    exit 1
fi

# Convert relative out-dir to absolute path under repo root.
case "$OUT_DIR" in
    /*) ABS_OUT="$OUT_DIR" ;;
    *)  ABS_OUT="$REPO_ROOT/$OUT_DIR" ;;
esac

export CFLAGS_wasm32_unknown_unknown="--sysroot=$SYSROOT"

cd "$WASM_CRATE"
exec wasm-pack build --target bundler -d "$ABS_OUT" "${EXTRA_ARGS[@]}" -- "${FEATURE_ARGS[@]}"
