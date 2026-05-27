import { defineConfig } from "vite-plus";

// `lint.options.typeCheck` runs `tsc --noEmit` during `vp check` so the
// TS surface is verified end-to-end on the same pipeline that runs
// Oxlint + the formatter check.
//
// `platform: 'neutral'` keeps the bundle isomorphic — the analyzer
// runs in the browser today, but the same code path is reachable from
// Node / Bun / Deno via the wasm-pack `--target web` shim.
//
// `deps.neverBundle` lists every package whose code should NOT be
// inlined into `dist/`. `@greycat/analyzer-wasm` ships the wasm-bindgen
// JS shim + `.wasm` binary; we re-export from it and let the consumer's
// bundler resolve the wasm asset.
export default defineConfig({
  lint: { ignorePatterns: ["dist/**"], options: { typeAware: true, typeCheck: true } },
  fmt: { ignorePatterns: ["dist/**"] },
  pack: {
    entry: {
      index: "src/index.ts",
      worker: "src/worker.ts",
    },
    format: ["esm"],
    dts: true,
    platform: "neutral",
    deps: { neverBundle: ["@greycat/analyzer-wasm", "comlink", "fflate"] },
  },
});
