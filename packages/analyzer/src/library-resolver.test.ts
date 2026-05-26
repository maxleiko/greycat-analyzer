// Unit tests for `RegistryLibraryResolver` + `MemoryLibraryCache`.
//
// Test surface:
//   - URL composition: channel + major.minor extraction across the
//     prerelease / bare-semver matrix.
//   - Happy-path resolve: fetch → unzip → InMemoryContext with the
//     `file:///lib/<name>/...` re-rooted URIs.
//   - In-memory dedup: concurrent `resolve(name, version)` calls fire
//     exactly one fetch.
//   - Persistent cache hit: a second resolve with the same key skips
//     the network entirely.
//   - `bypassCache` forces re-fetch.
//   - HTTP error surface.
//   - Wrapper-dir stripping for zips that embed `lib/<name>/...` at the
//     archive root.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { zipSync, strToU8 } from "fflate";

import { MemoryLibraryCache, RegistryLibraryResolver, registryUrlFor } from "./library-resolver.js";

function makeZip(files: Record<string, string>): Uint8Array {
  const entries: Record<string, Uint8Array> = {};
  for (const [path, source] of Object.entries(files)) {
    entries[path] = strToU8(source);
  }
  return zipSync(entries);
}

function okResponse(bytes: Uint8Array): Response {
  // `Response` clones the buffer on read, so the same fixture can be
  // reused across calls if a test wants to.
  return new Response(bytes.slice(), { status: 200 });
}

/** Yield for one microtask tick — enough time for a single pending
 *  `await` to settle. The dedup test uses this to observe state right
 *  after `resolveImpl` finishes its first `await this.cache.get(...)`
 *  and reaches the `this.fetchImpl(url)` call (before that call's own
 *  `await` suspends).
 *
 *  `queueMicrotask` (not `setTimeout(0)`) is deliberate: it stays on
 *  the microtask boundary, exposing implementation drift if a future
 *  edit slips another `await` between the cache read and the fetch.
 *  `setTimeout(0)` would mask that by hopping all the way to the next
 *  macrotask. */
function nextMicrotask(): Promise<void> {
  return new Promise((resolve) => {
    queueMicrotask(resolve);
  });
}

describe("registryUrlFor", () => {
  it("uses the prerelease tag as the channel segment", () => {
    expect(registryUrlFor("https://example.com/files", "std", "8.0.5-dev")).toBe(
      "https://example.com/files/std/dev/8.0/noarch/8.0.5-dev.zip",
    );
  });

  it("defaults bare semver to the `stable` channel", () => {
    expect(registryUrlFor("https://example.com/files", "std", "7.8.166")).toBe(
      "https://example.com/files/std/stable/7.8/noarch/7.8.166.zip",
    );
  });

  it("strips trailing slashes from the registry root", () => {
    expect(registryUrlFor("https://example.com/files/", "std", "7.8.166-stable")).toBe(
      "https://example.com/files/std/stable/7.8/noarch/7.8.166-stable.zip",
    );
  });

  it("encodes library names + versions for URL safety", () => {
    // `+` is a reserved URL char; encodeURIComponent → `%2B`. The
    // library name has a space (also unsafe). Channel parsing splits
    // on the FIRST `-`, so `1.0.0-build+local` → channel `build+local`.
    expect(registryUrlFor("https://example.com/files", "my lib", "1.0.0-build+local")).toBe(
      "https://example.com/files/my%20lib/build+local/1.0/noarch/1.0.0-build%2Blocal.zip",
    );
  });

  it("throws on versions without a major.minor", () => {
    expect(() => registryUrlFor("https://example.com/files", "std", "8")).toThrow(
      /invalid version/,
    );
  });
});

describe("RegistryLibraryResolver.resolve", () => {
  let fetchStub: ReturnType<typeof vi.fn>;
  let cache: MemoryLibraryCache;

  beforeEach(() => {
    cache = new MemoryLibraryCache();
    fetchStub = vi.fn();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("fetches the right URL, unzips, and returns an `file:///lib/<name>/...` Context", async () => {
    fetchStub.mockResolvedValueOnce(
      okResponse(
        makeZip({
          "core.gcl": "type Foo {}\n",
          "math/math.gcl": "fn pi(): float { return 3.14; }\n",
          // Non-.gcl entries are filtered out by the resolver.
          "README.md": "ignored",
        }),
      ),
    );
    const resolver = new RegistryLibraryResolver({
      registry: "https://example.com/files",
      fetch: fetchStub as unknown as typeof fetch,
      cache,
    });

    const ctx = await resolver.resolve("std", "7.8.166-stable");

    expect(fetchStub).toHaveBeenCalledTimes(1);
    expect(fetchStub).toHaveBeenCalledWith(
      "https://example.com/files/std/stable/7.8/noarch/7.8.166-stable.zip",
    );
    expect(ctx.read("file:///lib/std/core.gcl")).toBe("type Foo {}\n");
    expect(ctx.read("file:///lib/std/math/math.gcl")).toBe("fn pi(): float { return 3.14; }\n");
    expect(ctx.read("file:///lib/std/README.md")).toBeUndefined();
    expect([...ctx.uris()].sort()).toEqual([
      "file:///lib/std/core.gcl",
      "file:///lib/std/math/math.gcl",
    ]);
  });

  it("strips a `lib/<name>/` wrapper directory inside the zip", async () => {
    fetchStub.mockResolvedValueOnce(
      okResponse(
        makeZip({
          "lib/std/core.gcl": "type Foo {}\n",
          "lib/std/sub/x.gcl": "fn x() {}\n",
        }),
      ),
    );
    const resolver = new RegistryLibraryResolver({
      registry: "https://example.com/files",
      fetch: fetchStub as unknown as typeof fetch,
      cache,
    });

    const ctx = await resolver.resolve("std", "7.8.166-stable");

    expect(ctx.read("file:///lib/std/core.gcl")).toBe("type Foo {}\n");
    expect(ctx.read("file:///lib/std/sub/x.gcl")).toBe("fn x() {}\n");
  });

  it("de-duplicates concurrent resolves of the same library", async () => {
    // The stub resolves only when we explicitly release it — lets us
    // assert "second resolve started before the first finished".
    let release!: (response: Response) => void;
    const pending = new Promise<Response>((r) => {
      release = r;
    });
    fetchStub.mockReturnValueOnce(pending);
    const resolver = new RegistryLibraryResolver({
      registry: "https://example.com/files",
      fetch: fetchStub as unknown as typeof fetch,
      cache,
    });

    const a = resolver.resolve("std", "7.8.166-stable");
    const b = resolver.resolve("std", "7.8.166-stable");
    // `resolveImpl` runs synchronously up to `await this.cache.get(...)`.
    // One microtask tick later, the cache.get continuation runs and
    // reaches `this.fetchImpl(url)` (the call site, before its own
    // `await`). At that point fetchStub has been invoked exactly once.
    await nextMicrotask();
    expect(fetchStub).toHaveBeenCalledTimes(1);

    release(okResponse(makeZip({ "core.gcl": "type Foo {}\n" })));
    const [ctxA, ctxB] = await Promise.all([a, b]);
    expect(ctxA.read("file:///lib/std/core.gcl")).toBe("type Foo {}\n");
    expect(ctxB.read("file:///lib/std/core.gcl")).toBe("type Foo {}\n");
  });

  it("serves a second resolve from the persistent cache (no second fetch)", async () => {
    fetchStub.mockResolvedValueOnce(okResponse(makeZip({ "core.gcl": "type Foo {}\n" })));
    const resolver = new RegistryLibraryResolver({
      registry: "https://example.com/files",
      fetch: fetchStub as unknown as typeof fetch,
      cache,
    });

    await resolver.resolve("std", "7.8.166-stable");

    // A fresh resolver sharing the same cache MUST NOT re-fetch.
    const second = new RegistryLibraryResolver({
      registry: "https://example.com/files",
      fetch: fetchStub as unknown as typeof fetch,
      cache,
    });
    const ctx = await second.resolve("std", "7.8.166-stable");
    expect(fetchStub).toHaveBeenCalledTimes(1);
    expect(ctx.read("file:///lib/std/core.gcl")).toBe("type Foo {}\n");
  });

  it("`bypassCache` forces a re-fetch even when the cache has the key", async () => {
    fetchStub
      .mockResolvedValueOnce(okResponse(makeZip({ "core.gcl": "v1\n" })))
      .mockResolvedValueOnce(okResponse(makeZip({ "core.gcl": "v2\n" })));
    const warm = new RegistryLibraryResolver({
      registry: "https://example.com/files",
      fetch: fetchStub as unknown as typeof fetch,
      cache,
    });
    await warm.resolve("std", "7.8.166-stable");

    const bypassed = new RegistryLibraryResolver({
      registry: "https://example.com/files",
      fetch: fetchStub as unknown as typeof fetch,
      cache,
      bypassCache: true,
    });
    const ctx = await bypassed.resolve("std", "7.8.166-stable");

    expect(fetchStub).toHaveBeenCalledTimes(2);
    expect(ctx.read("file:///lib/std/core.gcl")).toBe("v2\n");
  });

  it("surfaces non-2xx responses as an error", async () => {
    fetchStub.mockResolvedValueOnce(new Response("not found", { status: 404 }));
    const resolver = new RegistryLibraryResolver({
      registry: "https://example.com/files",
      fetch: fetchStub as unknown as typeof fetch,
      cache,
    });

    await expect(resolver.resolve("std", "9.9.9-stable")).rejects.toThrow(/HTTP 404/);
  });
});

describe("MemoryLibraryCache", () => {
  it("round-trips files by key, returning a defensive copy", async () => {
    const cache = new MemoryLibraryCache();
    const files = new Map([["core.gcl", "type Foo {}\n"]]);
    await cache.set("std@1.0.0", files);

    // Mutating the input map shouldn't affect what `get` returns.
    files.set("intruder.gcl", "leak\n");

    const read = await cache.get("std@1.0.0");
    expect(read).not.toBeNull();
    expect(read!.size).toBe(1);
    expect(read!.get("core.gcl")).toBe("type Foo {}\n");
  });

  it("returns null on miss", async () => {
    const cache = new MemoryLibraryCache();
    expect(await cache.get("absent@0.0.0")).toBeNull();
  });
});
