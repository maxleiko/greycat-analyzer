import { defineConfig } from "vite-plus";

const r = (p: string) => new URL(p, import.meta.url).pathname;

export default defineConfig({
  lint: { ignorePatterns: ["dist/**"], options: { typeAware: true, typeCheck: true } },
  fmt: { ignorePatterns: ["dist/**"] },
  pack: {
    entry: ["src/index.ts"],
    format: ["esm"],
    dts: true,
    platform: "browser",
    deps: { neverBundle: ["@greycat/analyzer", "monaco-editor"] },
  },
  resolve: {
    alias: [{ find: "@greycat/analyzer", replacement: r("../analyzer") }],
  },
});
