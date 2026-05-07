import { defineConfig } from "vite-plus";

export default defineConfig({
  lint: { options: { typeAware: true, typeCheck: true } },

  // The wasm package lives at `../greycat-analyzer-wasm/pkg/` (one level
  // above the playground root). Vite's default `server.fs.allow` denies
  // anything outside the playground; widen it to the workspace root so
  // dev-mode imports of `greycat-analyzer-wasm` can fetch the .wasm
  // file. (When Vite refuses, it returns an HTML error page; the bytes
  // hit `WebAssembly.instantiate` and you get a "magic word" failure.)
  server: {
    fs: {
      allow: [".."],
    },
  },
});
