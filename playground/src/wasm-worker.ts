/// <reference lib="WebWorker" />

// Web Worker that hosts the wasm analyzer. Keeps every parse / lower /
// analyze off the main thread so Monaco stays responsive while the
// user types — and gives us a clean seam for routing Monaco language
// providers (hover / completion / diagnostics) through the same wasm
// surface in a follow-up.
//
// Wire protocol: every request from the main thread carries a
// monotonic numeric `id`, a `method` name (one of the wasm exports we
// surface), and a single string `source`. The worker replies with the
// same `id` plus either `result` (the wasm export's return value,
// already JSON-shaped via `serde-wasm-bindgen`) or `error` (a string).
// The response shape is defined in `analyzer-client.ts`; the two
// files stay in lockstep on the message contract.

import init, * as wasm from "greycat-analyzer-wasm";
import wasmUrl from "greycat-analyzer-wasm/greycat_analyzer_wasm_bg.wasm?url";

type AnalyzerWasm = typeof wasm;

type Method =
  | "diagnostics"
  | "tokens"
  | "parse_tree"
  | "parse_sexp"
  | "lower_hir"
  | "infer_types"
  | "format";

interface Request {
  id: number;
  method: Method;
  source: string;
}

interface Response {
  id: number;
  result?: unknown;
  error?: string;
}

const ready: Promise<AnalyzerWasm> = init({ module_or_path: wasmUrl }).then(() => wasm);

self.addEventListener("message", (ev: MessageEvent<Request>) => {
  const { id, method, source } = ev.data;
  ready
    .then((w) => {
      // The wasm exports take a single `source` string and return a
      // JSON-shaped value (or string for `format` / `parse_sexp`).
      const fn = w[method];
      if (typeof fn !== "function") {
        throw new Error(`unknown analyzer method: ${method}`);
      }
      // Direct invocation — every export shares the same arity.
      const result = (fn as (s: string) => unknown)(source);
      const reply: Response = { id, result };
      (self as DedicatedWorkerGlobalScope).postMessage(reply);
    })
    .catch((err: unknown) => {
      const reply: Response = {
        id,
        error: String((err as Error)?.message ?? err),
      };
      (self as DedicatedWorkerGlobalScope).postMessage(reply);
    });
});
