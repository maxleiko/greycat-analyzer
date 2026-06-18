#!/usr/bin/env bash
# Build greycat-analyzer-wasm with wasm-pack into a caller-specified
# directory. wasm-pack's output IS the published `@greycat/analyzer-
# wasm` package — `packages/analyzer-wasm/` is just a wasm-pack out-dir
# tracked in the workspace.
#
# Two consumer modes:
#
#   --out-dir packages/analyzer-wasm           # @greycat/analyzer-wasm npm pkg
#   --out-dir crates/wasm/pkg \                # playground (debug dumpers)
#       --features playground
#
# Emits `--target web`. The produced JS module exports an async
# `init()` that loads the `.wasm` via `new URL('..._bg.wasm',
# import.meta.url)` — a pattern Vite (and every other modern bundler)
# handles natively, no plugin required. `@greycat/analyzer` calls
# `init()` once on first `Project.create`, then the rest of the API
# stays synchronous.
#
# After wasm-pack writes the package.json, we patch the `name` field
# to `@greycat/analyzer-wasm`. wasm-pack's `--scope` flag PREPENDS
# `@<scope>/` to the Rust crate name (which is `greycat-analyzer-wasm`)
# and would give `@greycat/greycat-analyzer-wasm` — not what we want.
# A one-line in-place edit on the generated json is the cleanest way
# to land on the scoped name without renaming the Rust crate.
#
# We also rewrite `from 'env'` → `from '@greycat/analyzer-wasm-env'`
# in the wasm-bindgen JS. The `env` import comes from tree-sitter's C
# scanner referencing `<wctype.h>` predicates (`iswalpha`); wasm-
# bindgen's JS imports those from a module named `env`, which isn't
# a real package. Pointing the import at our sibling workspace pkg
# `@greycat/analyzer-wasm-env` makes node_modules resolution work
# everywhere — no bundler aliases required downstream.
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
WASM_CRATE="$REPO_ROOT/crates/wasm"

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
wasm-pack build --target web -d "$ABS_OUT" "${EXTRA_ARGS[@]}" -- "${FEATURE_ARGS[@]}"

# Patch the generated `name` field to the scoped form + add the env-
# shim dep so node_modules resolution picks up `iswalpha`. Only
# touches the npm-package output (not the playground's debug-dumper
# out-dir, which doesn't ship a scoped name).
PKG_JSON="$ABS_OUT/package.json"
if [ -f "$PKG_JSON" ] && grep -q '"name": "greycat-analyzer-wasm"' "$PKG_JSON"; then
    sed -i 's|"name": "greycat-analyzer-wasm"|"name": "@greycat/analyzer-wasm"|' "$PKG_JSON"
    sed -i 's|"type": "module",|"type": "module",\n  "dependencies": {\n    "@greycat/analyzer-wasm-env": "workspace:*"\n  },|' "$PKG_JSON"
fi

# Rewrite the `from 'env'` import inside the wasm-bindgen JS to point
# at the workspace env-shim package. wasm-bindgen always emits the
# bare `'env'` specifier; we replace it once with the real package
# name so consumers don't need a bundler alias.
JS_SHIM="$ABS_OUT/greycat_analyzer_wasm.js"
if [ -f "$JS_SHIM" ]; then
    sed -i "s|from 'env'|from '@greycat/analyzer-wasm-env'|" "$JS_SHIM"
fi
