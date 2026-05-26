import { defineConfig } from "vite-plus";
import { wasm } from "rolldown-plugin-wasm";

// `lint.options.typeCheck` runs `tsc --noEmit` during `vp check` so the
// TS surface is verified end-to-end (every wasm-bridge type, every
// Context implementation, every Project method signature) on the same
// pipeline that runs Oxlint + the formatter check.
//
// Library packaging via `vp pack` (wraps tsdown / Rolldown). Outputs ESM
// `dist/index.js` + `dist/worker.js` + `dist/stdlib.js` with per-entry
// `.d.ts` files; the `.wasm` binary lives alongside under `wasm/` and is
// shipped as-is via the package.json `files` allowlist.
//
// `platform: 'neutral'` keeps the bundle isomorphic — the analyzer runs
// in the browser today, but the same code path is reachable from Node /
// Bun / Deno once the wasm-pack `--target bundler` glue is loaded by
// the consumer's bundler. `rolldown-plugin-wasm` makes `import * as wasm
// from './wasm/greycat_analyzer_wasm.js'` resolve the sibling `.wasm`
// file at bundle time.
export default defineConfig({
  lint: { options: { typeAware: true, typeCheck: true } },
  pack: {
    entry: {
      index: "src/index.ts",
      worker: "src/worker.ts",
    },
    format: ["esm"],
    dts: true,
    platform: "neutral",
    external: ["comlink", "fflate"],
    plugins: [wasm()],
  },
});
