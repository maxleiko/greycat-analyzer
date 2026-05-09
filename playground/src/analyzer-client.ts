// Main-thread client for the wasm worker (`wasm-worker.ts`). Owns a
// singleton Worker, dispatches typed method calls, and routes replies
// back to the issuing call's promise via a numeric `id`.
//
// Vite's `?worker` import suffix bundles the worker module + spins
// it up as a `new Worker(..., { type: "module" })` for us.

import AnalyzerWorker from "./wasm-worker.ts?worker";

interface Response<T = unknown> {
  id: number;
  result?: T;
  error?: string;
}

type Method =
  | "diagnostics"
  | "tokens"
  | "parse_tree"
  | "parse_sexp"
  | "lower_hir"
  | "infer_types"
  | "format";

class AnalyzerClient {
  private worker: Worker;
  private pending = new Map<number, (res: Response) => void>();
  private nextId = 1;

  constructor() {
    this.worker = new AnalyzerWorker();
    this.worker.addEventListener("message", (ev: MessageEvent<Response>) => {
      const handler = this.pending.get(ev.data.id);
      if (!handler) return;
      this.pending.delete(ev.data.id);
      handler(ev.data);
    });
  }

  call<T = unknown>(method: Method, source: string): Promise<T> {
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, (res) => {
        if (res.error) {
          reject(new Error(res.error));
        } else {
          resolve(res.result as T);
        }
      });
      this.worker.postMessage({ id, method, source });
    });
  }

  // Typed one-liners — every panel calls one of these. Keeping them
  // co-located with the worker's `Method` union keeps the contract
  // honest at compile time.
  diagnostics(source: string) {
    return this.call("diagnostics", source);
  }
  tokens(source: string) {
    return this.call("tokens", source);
  }
  parse_tree(source: string) {
    return this.call("parse_tree", source);
  }
  parse_sexp(source: string) {
    return this.call<string>("parse_sexp", source);
  }
  lower_hir(source: string) {
    return this.call("lower_hir", source);
  }
  infer_types(source: string) {
    return this.call("infer_types", source);
  }
  format(source: string) {
    return this.call<string>("format", source);
  }
}

let singleton: AnalyzerClient | undefined;

export function getAnalyzer(): AnalyzerClient {
  if (!singleton) singleton = new AnalyzerClient();
  return singleton;
}

export type Analyzer = AnalyzerClient;
