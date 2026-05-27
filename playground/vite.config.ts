import { defineConfig } from "vite-plus";

const r = (p: string) => new URL(p, import.meta.url).pathname;

// Resolve `@greycat/*` to source so dev mode + the TS server don't
// require a prior `pnpm build:packages`. Mirrors `tsconfig.json`'s
// `paths` map. `@greycat/analyzer-wasm` + `@greycat/analyzer-wasm-env`
// still resolve normally through the pnpm workspace symlink + each
// package's own `package.json`.
//
// Vite handles the `.wasm` itself via the wasm-pack `--target web`
// `new URL(..., import.meta.url)` pattern — no plugin required.
//
// `server.fs.allow: [".."]` widens the dev-server filesystem
// allowlist to the workspace root so the wasm asset under
// `packages/analyzer-wasm/` is fetchable.
export default defineConfig({
  lint: { options: { typeAware: true, typeCheck: true } },

  server: {
    fs: {
      allow: [".."],
    },
  },

  resolve: {
    alias: [
      { find: "@greycat/analyzer", replacement: r("../packages/analyzer/src/index.ts") },
      { find: "@greycat/monaco", replacement: r("../packages/monaco/src/index.ts") },
    ],
  },
});
