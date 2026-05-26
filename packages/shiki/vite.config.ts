import { defineConfig } from "vite-plus";

export default defineConfig({
  lint: { options: { typeAware: true, typeCheck: true } },
  pack: {
    entry: {
      index: "src/index.ts",
      grammar: "src/grammar.ts",
    },
    format: ["esm"],
    dts: true,
    platform: "neutral",
    external: ["shiki"],
  },
});
