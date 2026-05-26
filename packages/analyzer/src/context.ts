// File IO abstraction for the analyzer. Mirrors the Rust-side
// `Context` trait at [`greycat-analyzer-core/src/resolver.rs`] — file
// reads are SYNCHRONOUS so the analyzer's resolver walk doesn't have
// to be re-engineered for async. The asynchronous concern (registry
// fetches for `@library` resolution) lives in `LibraryResolver`.
//
// Three prebuilt implementations cover the common cases:
//
// - `InMemoryContext` — back a Context by a `Map<uri, source>`. The
//   result of `RegistryLibraryResolver.resolve(...)` is wrapped in
//   one of these.
// - `MonacoContext` — read live from Monaco TextModels. The editor
//   owns the source-of-truth; the Context never holds a stale copy.
// - `mergeContexts([a, b, c])` — first hit wins. Compose
//   stdlib (InMemoryContext) with project (MonacoContext) without
//   either knowing about the other.
//
// Browser-only types (`monaco.editor.IStandaloneCodeEditor`,
// `monaco.editor.ITextModel`) are referenced via structural minimal
// interfaces so `MonacoContext` doesn't pull `monaco-editor` into the
// type-resolution graph of consumers that don't use it.

/** Sync file IO contract consumed by the analyzer. URIs are the same
 *  strings the wasm `Project` uses as keys (`file:///lib/std/core.gcl`,
 *  `file:///proj/main.gcl`, …). */
export interface Context {
  /** Return the source text at `uri`, or `undefined` if the Context
   *  doesn't know about that URI. Throws only for genuinely
   *  unexpected errors (corrupt model, etc.) — "not found" is
   *  `undefined`, not a throw. */
  read(uri: string): string | undefined;

  /** Every URI the Context can produce a `read` for. Used at project
   *  construction time to flatten the Context into the wasm `Project`'s
   *  file map. Implementations should return a stable iteration order
   *  to keep diagnostics output deterministic across calls. */
  uris(): Iterable<string>;
}

/** In-memory Context backed by a fixed `Map<uri, source>`. Returned by
 *  `RegistryLibraryResolver.resolve(...)` once a library's files are
 *  fetched + decoded. */
export class InMemoryContext implements Context {
  private readonly files: Map<string, string>;

  constructor(files: Map<string, string> | Record<string, string>) {
    this.files = files instanceof Map ? files : new Map(Object.entries(files));
  }

  read(uri: string): string | undefined {
    return this.files.get(uri);
  }

  uris(): Iterable<string> {
    return this.files.keys();
  }
}

/** Minimal structural view of `monaco.editor` — covers just the bits
 *  `MonacoContext` calls. Keeps `monaco-editor` out of the type graph
 *  for consumers that bring their own Context. */
export interface MonacoEditorNamespace {
  getModel(uri: { toString(): string } | string): MonacoModel | null;
  getModels(): MonacoModel[];
}

interface MonacoModel {
  uri: { toString(): string };
  getValue(): string;
}

/** Context backed by Monaco's TextModel registry. Reads pass through
 *  to `monaco.editor.getModel(uri)?.getValue()` on every call — Monaco
 *  owns the source-of-truth and the Context never goes stale. */
export class MonacoContext implements Context {
  constructor(
    private readonly monacoEditor: MonacoEditorNamespace,
    /** Filter URIs the Context exposes via `uris()`. Default: every
     *  loaded model. Use this to scope the Context to a specific
     *  project tree (e.g. `(uri) => uri.startsWith("file:///proj/")`)
     *  so unrelated open buffers don't leak in. */
    private readonly filter: (uri: string) => boolean = () => true,
  ) {}

  read(uri: string): string | undefined {
    const model = this.monacoEditor.getModel(uri);
    return model?.getValue();
  }

  *uris(): Iterable<string> {
    for (const model of this.monacoEditor.getModels()) {
      const uri = model.uri.toString();
      if (this.filter(uri)) {
        yield uri;
      }
    }
  }
}

/** First-hit-wins composition. Earlier Contexts take precedence on
 *  conflicting URIs — handy for stacking stdlib (immutable, in-memory)
 *  under live editor models (mutable, Monaco-backed). */
export function mergeContexts(contexts: readonly Context[]): Context {
  return {
    read(uri) {
      for (const ctx of contexts) {
        const source = ctx.read(uri);
        if (source !== undefined) {
          return source;
        }
      }
      return undefined;
    },
    *uris() {
      const seen = new Set<string>();
      for (const ctx of contexts) {
        for (const uri of ctx.uris()) {
          if (!seen.has(uri)) {
            seen.add(uri);
            yield uri;
          }
        }
      }
    },
  };
}
