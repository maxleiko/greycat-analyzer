// JS-side `Project` wrapper around the wasm `Project` handle. Owns
// the reactive lifecycle that the wasm side intentionally doesn't:
//
// - Parses every `@library("name", "version")` pragma in the project's
//   entrypoint source on construction + on every `didChange`.
// - Calls the `LibraryResolver` to load any newly-referenced library
//   version. Resolved Contexts get folded into the project's effective
//   file map.
// - Reads each file's source through the `Context` chain (stdlib +
//   project + caller-merged), assembles a `Map<uri, source>`, and
//   passes it to the wasm `Project` constructor / reconstructor.
//
// The wasm `Project` itself is recreated whenever the library set
// changes — a full rebuild is cheap relative to the time spent in
// `await libraries.resolve(...)`. For pure body edits (no `@library`
// pragma change) we forward to the wasm Project's `change(uri, source)`
// method so its internal HIR / signatures cache invalidates per-URI
// instead of from scratch.

import wasmInit, {
  Project as WasmProject,
  type CodeAction,
  type CompletionList,
  type Diagnostic,
  type DocumentHighlight,
  type DocumentSymbol,
  type FoldingRange,
  type Hover,
  type Range,
  type InlayHint,
  type Location,
  type RenameTarget,
  type SemanticTokens,
  type SignatureHelp,
  type TextEdit,
  type WorkspaceSymbol,
} from "@greycat/analyzer-wasm";

import type { Context } from "./context.js";
import { InMemoryContext, mergeContexts } from "./context.js";
import type { LibraryResolver } from "./library-resolver.js";

export interface ProjectCreateOptions {
  /** Entrypoint URI — typically `file:///project.gcl`. */
  entrypoint: string;
  /** File IO contract. The Context drives every source read; the
   *  entrypoint and every `@include`d file are read through it. */
  context: Context;
  /** Async library resolution. Called with each `(name, version)` the
   *  project's `@library` pragmas reference. */
  libraries: LibraryResolver;
}

/** Top-of-file pragma; the grammar fixes the shape so a regex is
 *  enough here (we don't have a wasm `Project` yet at the point this
 *  runs — chicken/egg). The pragma can be wrapped in arbitrary
 *  whitespace, including newlines between the args. */
const LIBRARY_PRAGMA = /@library\s*\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*\)/g;

interface LibrarySpec {
  name: string;
  version: string;
}

function librarySpecsFrom(source: string): LibrarySpec[] {
  const out: LibrarySpec[] = [];
  for (const m of source.matchAll(LIBRARY_PRAGMA)) {
    out.push({ name: m[1]!, version: m[2]! });
  }
  return out;
}

function specKey(spec: LibrarySpec): string {
  return `${spec.name}@${spec.version}`;
}

// One-shot wasm-pack `--target web` init. `wasmInit()` is idempotent
// (returns the cached module if already loaded), but we cache the
// promise here so concurrent `Project.create` calls share a single
// awaited boot instead of racing the fetch.
let wasmReady: Promise<unknown> | undefined;
function ensureWasmReady(): Promise<unknown> {
  if (wasmReady === undefined) {
    wasmReady = wasmInit();
  }
  return wasmReady;
}

/** Reactive analyzer handle. Construction is async because the
 *  initial `@library` resolution is async; analysis calls (hover,
 *  completion, …) are synchronous against the cached wasm Project.
 *
 *  Call `didChange(uri)` whenever the host's source changes. For pure
 *  body edits it's near-instant (wasm-side per-URI invalidation); when
 *  the entrypoint's `@library` set changes it awaits the resolver. */
export class Project {
  /** Boot the wasm module (idempotent), then construct + wait for
   *  initial library resolution. */
  static async create(options: ProjectCreateOptions): Promise<Project> {
    await ensureWasmReady();
    const project = new Project(options);
    await project.refresh();
    return project;
  }

  private readonly entrypoint: string;
  private readonly callerContext: Context;
  private readonly libraries: LibraryResolver;
  /** `name@version` → resolved Context. Populated on `refresh()`. */
  private readonly libraryContexts = new Map<string, Context>();
  /** Spec list as of the most recent successful refresh. */
  private currentSpecs: LibrarySpec[] = [];
  /** Wasm handle. Recreated whenever the library set changes; mutated
   *  in-place via `change(uri, source)` for pure body edits. */
  private wasm!: WasmProject;

  private constructor(options: ProjectCreateOptions) {
    this.entrypoint = options.entrypoint;
    this.callerContext = options.context;
    this.libraries = options.libraries;
  }

  /** Re-read the entrypoint, diff the `@library` set against the
   *  previous refresh, fetch any new versions, then either rebuild or
   *  fast-path-invalidate the wasm Project. */
  async didChange(uri: string): Promise<void> {
    if (uri === this.entrypoint) {
      await this.refresh();
      return;
    }
    // Non-entrypoint edit — body change to an included / library file.
    // The library set can't shift from here, so the wasm Project's
    // per-URI invalidation is sufficient. The Context layer is
    // authoritative on the new source; pull it through.
    const source = this.callerContext.read(uri);
    if (source !== undefined) {
      this.wasm.change(uri, source);
    }
  }

  private async refresh(): Promise<void> {
    const entrypointSource = this.callerContext.read(this.entrypoint);
    if (entrypointSource === undefined) {
      throw new Error(
        `Project: entrypoint "${this.entrypoint}" not present in the supplied Context`,
      );
    }

    const newSpecs = librarySpecsFrom(entrypointSource);
    const newKeys = new Set(newSpecs.map(specKey));
    const oldKeys = new Set(this.currentSpecs.map(specKey));

    // Resolve any library not already in `libraryContexts`. We hit the
    // resolver in parallel — the LibraryResolver is expected to
    // de-duplicate concurrent calls itself (the bundled
    // `RegistryLibraryResolver` does).
    const toResolve = newSpecs.filter((s) => !this.libraryContexts.has(specKey(s)));
    const resolved = await Promise.all(
      toResolve.map(
        async (s) => [specKey(s), await this.libraries.resolve(s.name, s.version)] as const,
      ),
    );
    for (const [key, ctx] of resolved) {
      this.libraryContexts.set(key, ctx);
    }

    // Drop libraries that were removed from the entrypoint. Keeping
    // them around would leak unused stdlibs across version flips.
    // Collect into an array first so the in-iteration `delete` doesn't
    // mutate the live `Map.keys()` iterator.
    const stale: string[] = [];
    for (const key of this.libraryContexts.keys()) {
      if (!newKeys.has(key)) {
        stale.push(key);
      }
    }
    for (const key of stale) {
      this.libraryContexts.delete(key);
    }

    const changed = newKeys.size !== oldKeys.size || [...newKeys].some((k) => !oldKeys.has(k));
    if (this.wasm && !changed) {
      // Pure body edit on the entrypoint — fast path.
      this.wasm.change(this.entrypoint, entrypointSource);
      return;
    }

    // Library set shifted (first construction also lands here).
    // Rebuild the wasm Project from the merged Context.
    const merged = mergeContexts([
      ...this.libraryContexts.values(),
      this.callerContext,
      new InMemoryContext({ [this.entrypoint]: entrypointSource }),
    ]);
    const files = new Map<string, string>();
    for (const uri of merged.uris()) {
      const source = merged.read(uri);
      if (source !== undefined) {
        files.set(uri, source);
      }
    }
    this.wasm = new WasmProject(this.entrypoint, files);
    this.currentSpecs = newSpecs;
  }

  // -- Forwarded LSP / IDE methods --------------------------------------------

  diagnostics(uri: string): Diagnostic[] {
    return this.wasm.diagnostics(uri);
  }

  hover(uri: string, line: number, character: number): Hover | undefined {
    return this.wasm.hover(uri, line, character);
  }

  completion(uri: string, line: number, character: number): CompletionList | undefined {
    return this.wasm.completion(uri, line, character);
  }

  signatureHelp(uri: string, line: number, character: number): SignatureHelp | undefined {
    return this.wasm.signatureHelp(uri, line, character);
  }

  inlayHints(
    uri: string,
    startLine: number,
    startCharacter: number,
    endLine: number,
    endCharacter: number,
  ): InlayHint[] {
    return this.wasm.inlayHints(uri, startLine, startCharacter, endLine, endCharacter);
  }

  foldingRanges(uri: string): FoldingRange[] {
    return this.wasm.foldingRanges(uri);
  }

  documentHighlights(uri: string, line: number, character: number): DocumentHighlight[] {
    return this.wasm.documentHighlights(uri, line, character);
  }

  documentSymbols(uri: string): DocumentSymbol[] {
    return this.wasm.documentSymbols(uri);
  }

  workspaceSymbols(query: string): WorkspaceSymbol[] {
    return this.wasm.workspaceSymbols(query);
  }

  semanticTokens(uri: string): SemanticTokens {
    return this.wasm.semanticTokens(uri);
  }

  selectionRanges(uri: string, line: number, character: number): Range[] {
    return this.wasm.selectionRanges(uri, line, character);
  }

  codeActions(
    uri: string,
    startLine: number,
    startCharacter: number,
    endLine: number,
    endCharacter: number,
  ): CodeAction[] {
    return this.wasm.codeActions(uri, startLine, startCharacter, endLine, endCharacter);
  }

  references(uri: string, line: number, character: number): Location[] {
    return this.wasm.references(uri, line, character);
  }

  resolveRenameTarget(uri: string, line: number, character: number): RenameTarget | undefined {
    return this.wasm.resolveRenameTarget(uri, line, character);
  }

  renameTargetSites(target: RenameTarget): Location[] {
    return this.wasm.renameTargetSites(target);
  }

  format(uri: string): TextEdit[] {
    return this.wasm.format(uri);
  }
}
