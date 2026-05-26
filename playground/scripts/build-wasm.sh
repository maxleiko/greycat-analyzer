#!/usr/bin/env bash
# Build greycat-analyzer-wasm and emit the JS bindings into the local
# `../greycat-analyzer-wasm/pkg/` directory the playground links from.
#
# wasm-pack drives `cargo build --target wasm32-unknown-unknown`, which
# in turn builds tree-sitter-greycat's parser.c via clang. Clang for
# wasm32-unknown-unknown has no libc by default, so we point it at the
# Emscripten sysroot for the missing headers (`stdlib.h`, etc.).
#
# Honors $EMSDK if set; otherwise falls back to the default
# `~/app/emsdk` path used in the maintainer's environment. Override
# with `EMSDK=/path/to/emsdk ./scripts/build-wasm.sh` if needed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)/.."
WASM_CRATE="$REPO_ROOT/greycat-analyzer-wasm"

EMSDK_ROOT="${EMSDK:-${EMSDK_ROOT:-$HOME/app/emsdk}}"
SYSROOT="$EMSDK_ROOT/upstream/emscripten/cache/sysroot"

if [ ! -d "$SYSROOT" ]; then
    echo "error: missing emscripten sysroot at $SYSROOT" >&2
    echo "       set EMSDK=/path/to/emsdk and re-run" >&2
    exit 1
fi

export CFLAGS_wasm32_unknown_unknown="--sysroot=$SYSROOT"

cd "$WASM_CRATE"
# `--features playground` enables the CST / HIR / tokens / types / diagnostics
# / format dumpers the playground UI consumes. The published `@greycat/analyzer`
# package is built without this feature.
exec wasm-pack build --target web -d pkg -- --features playground "$@"
