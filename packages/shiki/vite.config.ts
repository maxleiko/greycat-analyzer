import { defineConfig } from "vite-plus";

export default defineConfig({
  lint: {
    ignorePatterns: ["dist/**", "src/grammar.generated.ts"],
    options: { typeAware: true, typeCheck: true },
  },
  fmt: { ignorePatterns: ["dist/**", "src/grammar.generated.ts"] },
  pack: {
    entry: {
      index: "src/index.ts",
      grammar: "src/grammar.ts",
    },
    format: ["esm"],
    dts: true,
    platform: "neutral",
    deps: { neverBundle: ["shiki"] },
  },
});
