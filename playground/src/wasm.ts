// Lazy wasm loader. Initializes greycat-analyzer-wasm once and caches
// the module. Every panel imports `getWasm()` and calls the named
// exports as ordinary functions.
//
// We pass the .wasm URL to `init()` explicitly via Vite's `?url` import
// suffix so resolution doesn't depend on wasm-bindgen's default
// `import.meta.url`-relative fetch (which routinely fails when the .wasm
// file isn't co-located with the JS bundle, as is the case under Vite's
// transformed module graph).

import init, * as wasm from "greycat-analyzer-wasm";
import wasmUrl from "greycat-analyzer-wasm/greycat_analyzer_wasm_bg.wasm?url";

let ready: Promise<typeof wasm> | undefined;

export function getWasm(): Promise<typeof wasm> {
  if (!ready) {
    ready = init({ module_or_path: wasmUrl }).then(() => wasm);
  }
  return ready;
}
