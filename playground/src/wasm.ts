// Lazy wasm loader. Initializes greycat-analyzer-wasm once and caches
// the module. Every panel imports `getWasm()` and calls the named
// exports as ordinary functions.

import init, * as wasm from "greycat-analyzer-wasm";

let ready: Promise<typeof wasm> | undefined;

export function getWasm(): Promise<typeof wasm> {
  if (!ready) {
    ready = init().then(() => wasm);
  }
  return ready;
}
