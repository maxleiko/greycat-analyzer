import { defineConfig } from "vite-plus";

export default defineConfig({
  lint: { options: { typeAware: true, typeCheck: true } },
  pack: {
    entry: ["src/index.ts"],
    format: ["esm"],
    dts: true,
    platform: "browser",
    external: ["@greycat/analyzer", "monaco-editor"],
  },
});
