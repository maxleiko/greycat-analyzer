// Async library resolution — fetch a `@library("name", "version")`
// closure from the GreyCat registry, decode the `.zip` payload, cache
// it for repeat lookups, and surface the result as a `Context` the
// analyzer can consume synchronously.
//
// Registry URL convention (today): `.zip` archives served at
//   `<registry>/<lib>/<channel>/<major.minor>/noarch/<version>.zip`
// where `<channel>` is the prerelease tag (`stable`, `dev`, `beta`)
// and `<major.minor>` is the leading two version components. The
// registry rewrite to `.json.gz` is a future change; once it lands
// this resolver can swap its decode path without touching its
// contract.
//
// ZIP decoding uses `fflate` because the Web platform exposes only
// the gzip / deflate stream APIs (via `DecompressionStream`), not the
// ZIP archive container. Cost: ~8 KB gzipped.
//
// Two layers of cache:
//   1. In-memory `Map<"name@version", Context>` per session (fast path
//      for re-fetches inside the same process).
//   2. Pluggable persistent `LibraryCache`. Defaults to
//      `IndexedDbLibraryCache` in browsers; `NoopLibraryCache`
//      elsewhere. Tests inject `MemoryLibraryCache` (a thin shim).

import { unzipSync } from "fflate";

import type { Context } from "./context.js";
import { InMemoryContext } from "./context.js";

/** Async library resolution contract — anyone can implement this
 *  (custom mirrors, offline caches, test stubs). The analyzer's
 *  reactive lifecycle calls `resolve` whenever a project's `@library`
 *  set changes. */
export interface LibraryResolver {
  /** Resolve `name@version` to a Context containing every file in the
   *  library's closure. Throws if the library can't be loaded (and the
   *  consumer surfaces a project-level diagnostic). */
  resolve(name: string, version: string): Promise<Context>;
}

/** Pluggable persistent cache for resolved libraries. Default
 *  implementation is `IndexedDbLibraryCache` in browsers, no-op
 *  elsewhere. */
export interface LibraryCache {
  get(key: string): Promise<Map<string, string> | null>;
  set(key: string, files: Map<string, string>): Promise<void>;
}

export interface RegistryLibraryResolverOptions {
  /** Defaults to `https://get.greycat.io/files/`. */
  registry?: string;
  /** Force re-fetch ignoring both in-memory and persistent cache. */
  bypassCache?: boolean;
  /** Override the persistent cache. Default: IndexedDB in browsers,
   *  no-op elsewhere. Useful for tests (inject `MemoryLibraryCache`)
   *  or non-browser hosts that want their own persistence. */
  cache?: LibraryCache;
  /** Override the `fetch` implementation. Defaults to `globalThis.fetch`.
   *  Useful for tests + non-browser hosts with a custom fetcher. */
  fetch?: typeof fetch;
}

const DEFAULT_REGISTRY = "https://get.greycat.io/files/";

/** The default registry-backed implementation. Plug into
 *  `Project.create({ libraries: new RegistryLibraryResolver() })`. */
export class RegistryLibraryResolver implements LibraryResolver {
  private readonly registry: string;
  private readonly bypassCache: boolean;
  private readonly cache: LibraryCache;
  private readonly fetchImpl: typeof fetch;
  private readonly memoryCache = new Map<string, Promise<Context>>();

  constructor(options: RegistryLibraryResolverOptions = {}) {
    this.registry = (options.registry ?? DEFAULT_REGISTRY).replace(/\/+$/, "");
    this.bypassCache = options.bypassCache ?? false;
    this.cache = options.cache ?? defaultCache();
    this.fetchImpl = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  resolve(name: string, version: string): Promise<Context> {
    const key = cacheKey(name, version);
    // De-duplicate concurrent resolves of the same library — multiple
    // `didChange` calls landing back-to-back shouldn't fire two
    // fetches.
    const inflight = this.memoryCache.get(key);
    if (inflight && !this.bypassCache) {
      return inflight;
    }
    const fresh = this.resolveImpl(name, version);
    this.memoryCache.set(key, fresh);
    return fresh;
  }

  private async resolveImpl(name: string, version: string): Promise<Context> {
    const key = cacheKey(name, version);

    if (!this.bypassCache) {
      const cached = await this.cache.get(key);
      if (cached) {
        return new InMemoryContext(rerootFiles(name, cached));
      }
    }

    const url = registryUrlFor(this.registry, name, version);
    const response = await this.fetchImpl(url);
    if (!response.ok) {
      throw new Error(`RegistryLibraryResolver: HTTP ${response.status} fetching ${url}`);
    }
    const buffer = await response.arrayBuffer();
    const files = unzipToMap(new Uint8Array(buffer));
    await this.cache.set(key, files);
    return new InMemoryContext(rerootFiles(name, files));
  }
}

// --- URL / archive helpers ---------------------------------------------------

/** Compose the registry URL for `name@version`. Exposed for tests +
 *  consumers that want to introspect what the resolver would fetch. */
export function registryUrlFor(registry: string, name: string, version: string): string {
  const root = registry.replace(/\/+$/, "");
  const channel = extractChannel(version);
  const majorMinor = extractMajorMinor(version);
  return `${root}/${encodeURIComponent(name)}/${channel}/${majorMinor}/noarch/${encodeURIComponent(version)}.zip`;
}

function extractChannel(version: string): string {
  // `7.8.166-stable` → "stable"; `8.0.5-dev` → "dev"; bare semver →
  // "stable" by convention.
  const dash = version.indexOf("-");
  return dash < 0 ? "stable" : version.slice(dash + 1);
}

function extractMajorMinor(version: string): string {
  const parts = version.split(".");
  if (parts.length < 2) {
    throw new Error(`RegistryLibraryResolver: invalid version "${version}"`);
  }
  return `${parts[0]}.${parts[1]}`;
}

function cacheKey(name: string, version: string): string {
  return `${name}@${version}`;
}

/** Extract every `.gcl` entry from `zipBytes`. The returned `Map`'s
 *  keys are *zip-relative paths* (e.g. `core.gcl`, `io/socket.gcl`);
 *  `rerootFiles` applies the `file:///lib/<name>/` URI prefix at the
 *  edge so the cache stays library-name-agnostic. */
function unzipToMap(zipBytes: Uint8Array): Map<string, string> {
  const decoder = new TextDecoder("utf-8");
  const unzipped = unzipSync(zipBytes, {
    filter(file) {
      return file.name.endsWith(".gcl");
    },
  });
  const out = new Map<string, string>();
  for (const [name, bytes] of Object.entries(unzipped)) {
    // The zip may carry a top-level `lib/std/` (or similar) wrapper
    // directory; strip it so cached paths are stable regardless of
    // packaging tweaks on the server side.
    const normalized = stripWrapperDir(name);
    out.set(normalized, decoder.decode(bytes));
  }
  return out;
}

function stripWrapperDir(zipPath: string): string {
  // Three layouts we tolerate:
  //   1. Flat — `core.gcl`, `math/math.gcl` (return verbatim).
  //   2. Top-level wrapper — `lib/<libname>/...` (no leading slash).
  //   3. Nested wrapper — `<prefix>/lib/<libname>/...`.
  // Keep everything after the `<libname>/` segment.
  let markerEnd: number;
  if (zipPath.startsWith("lib/")) {
    markerEnd = "lib/".length;
  } else {
    const nested = zipPath.indexOf("/lib/");
    if (nested < 0) {
      return zipPath;
    }
    markerEnd = nested + "/lib/".length;
  }
  const after = zipPath.slice(markerEnd);
  const slash = after.indexOf("/");
  return slash < 0 ? after : after.slice(slash + 1);
}

function rerootFiles(name: string, files: Map<string, string>): Map<string, string> {
  const rerooted = new Map<string, string>();
  for (const [relPath, source] of files) {
    rerooted.set(`file:///lib/${name}/${relPath}`, source);
  }
  return rerooted;
}

// --- Cache implementations ---------------------------------------------------

/** In-memory `LibraryCache`. Useful for tests + sessions where
 *  IndexedDB is unavailable but caller still wants de-duplication. */
export class MemoryLibraryCache implements LibraryCache {
  private readonly store = new Map<string, Map<string, string>>();

  get(key: string): Promise<Map<string, string> | null> {
    const entry = this.store.get(key);
    return Promise.resolve(entry ? new Map(entry) : null);
  }

  set(key: string, files: Map<string, string>): Promise<void> {
    this.store.set(key, new Map(files));
    return Promise.resolve();
  }
}

/** No-op cache. Used when IndexedDB is unavailable (Node default). */
export class NoopLibraryCache implements LibraryCache {
  get(): Promise<Map<string, string> | null> {
    return Promise.resolve(null);
  }
  set(): Promise<void> {
    return Promise.resolve();
  }
}

const IDB_STORE = "by-library";
const DEFAULT_IDB_NAME = "greycat-analyzer-libraries";

/** Browser-only IndexedDB cache. Survives reloads. */
export class IndexedDbLibraryCache implements LibraryCache {
  constructor(private readonly dbName: string = DEFAULT_IDB_NAME) {}

  async get(key: string): Promise<Map<string, string> | null> {
    const db = await openDb(this.dbName);
    if (!db) {
      return null;
    }
    return await new Promise<Map<string, string> | null>((resolve) => {
      const tx = db.transaction(IDB_STORE, "readonly");
      const store = tx.objectStore(IDB_STORE);
      const req = store.get(key);
      req.onsuccess = () => {
        const raw = req.result as Record<string, string> | undefined;
        if (!raw) {
          resolve(null);
          return;
        }
        resolve(new Map(Object.entries(raw)));
      };
      req.onerror = () => {
        resolve(null);
      };
    });
  }

  async set(key: string, files: Map<string, string>): Promise<void> {
    const db = await openDb(this.dbName);
    if (!db) {
      return;
    }
    await new Promise<void>((resolve) => {
      const tx = db.transaction(IDB_STORE, "readwrite");
      const store = tx.objectStore(IDB_STORE);
      const obj: Record<string, string> = {};
      for (const [k, v] of files) {
        obj[k] = v;
      }
      const req = store.put(obj, key);
      req.onsuccess = () => {
        resolve();
      };
      req.onerror = () => {
        resolve();
      };
    });
  }
}

function defaultCache(): LibraryCache {
  return typeof indexedDB !== "undefined" ? new IndexedDbLibraryCache() : new NoopLibraryCache();
}

async function openDb(dbName: string): Promise<IDBDatabase | null> {
  if (typeof indexedDB === "undefined") {
    return null;
  }
  return await new Promise<IDBDatabase | null>((resolve, reject) => {
    const req = indexedDB.open(dbName, 1);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(IDB_STORE)) {
        db.createObjectStore(IDB_STORE);
      }
    };
    req.onsuccess = () => {
      resolve(req.result);
    };
    req.onerror = () => {
      reject(req.error as Error);
    };
  });
}
