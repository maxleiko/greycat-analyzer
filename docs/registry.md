# GreyCat registry — requirements for the rewrite

Notes from porting `@library` completion to the Rust analyzer (P15.3).
Captures what the *consumer* (LSP / IDE / CI tooling) needs from the
new registry to make library discovery, version selection, and project
hygiene first-class. The current registry exposes a directory listing
and nothing else; everything else has to be reconstructed by walking
the tree, which dominates completion latency and blocks several
features outright.

The headline gap: **there is no way to enumerate libraries today.**
The completer can fetch versions of a *known* lib, but it can't list
candidate names — `@library("<cursor>", ...)` completion has no shape
to fall back on. A flat names index fixes that.

## What the analyzer needs

### 1. Library *name* listing

A flat, fast-to-fetch endpoint that returns every published library
name. This is the elephant in the room; without it `@library("|", …)`
name completion is impossible.

- **Endpoint shape** (suggestion): `GET /v1/libraries` →
  `{libraries: [{name, description?, latest_stable?, latest_dev?}, …]}`.
- **Tags or categories** (`db`, `ml`, `geospatial`, …) so editors can
  show grouping and filter by topic.
- **Visibility flag** distinguishing first-party libs from
  user-published ones, so the analyzer can surface "official" libs
  first in the popup.
- **Deprecation marker** so deprecated libs are still listed (for
  resolving existing `@library` lines) but ranked last and badged in
  the popup.
- Cache-friendly: weak ETag / Last-Modified, gzip, sub-100ms p50.

### 2. Per-library *version* listing — single round trip

The current shape forces 1 + N + N×M fetches: list branches → for each
branch list M.m → probe arch → list zips per (branch, M.m). For `core`
that's ~50–200 calls, all serial in the TS reference, parallelizable
in Rust but still bandwidth-heavy.

- **Endpoint shape**: `GET /v1/libraries/<name>/versions` →
  `{versions: [{version, channel, published_at, deprecated?, yanked?, …}, …]}`.
- One round trip, every version of every channel. p95 should be a
  single fast HTTPS call, no recursion.
- Sorted by the registry server (descending semver), so the client can
  paginate or display without re-sorting.
- **Per-version metadata** (see §3) so the editor can render rich
  completion items without follow-up requests.

### 3. Per-version metadata

For a great completion / install UX, every version row should
include:

- **`version`** — semver string, including prerelease channel
  (`8.0.5-dev`, `7.8.166-stable`).
- **`channel`** — `stable` / `dev` / `testing` / `experimental` /
  custom branch — already implied by the suffix today, but having it
  as a first-class field saves clients from string-suffix parsing.
- **`published_at`** — ISO-8601 UTC; surfaced in the LSP
  `labelDetails.description`.
- **`yanked: bool`** — versions removed for safety reasons. Editors
  should still resolve them for existing pins (so old projects don't
  break) but rank them last and badge them.
- **`deprecated: bool` + `deprecation_message?`** — for soft
  deprecations.
- **`compatible_runtime: { min, max? }`** — what runtime versions this
  lib version supports. Lets the analyzer warn when `@library`
  pins an incompatible version against the runtime in the project.
- **`compatible_archs: [string]`** — archs the lib publishes for.
  Drives the per-arch download URL and lets the editor warn when a
  pinned version isn't published for the user's platform.
- **`requires: [{name, version_constraint}]`** — declared
  dependencies on other GreyCat libraries (semver constraint).
  Foundation for transitive resolution + lockfiles (§7).
- **`description?`** — one-liner per release ("changelog headline").
  Surface in the LSP `documentation` field on the version completion
  item so users can see what changed without leaving the editor.
- **`size_bytes`** — for an "this download is X MB" footnote and to
  warn before pulling absurdly large libs.
- **`integrity`** — sha256 (or stronger). Lockfile + supply-chain
  defense (see §6).
- **`license`** + **`license_url`** — for the editor to surface and
  for `lint`/`audit` flows.
- **`source_url?`** — link to the source repo / commit. Hover and
  goto-source.

### 4. Channels as data, not string suffixes

Today the channel hides in the version's prerelease tag (`-stable`,
`-dev`). Tools have to string-split to reason about it. Make
`channel` a first-class field; the version core can be a pure semver
without the suffix dance:

- `version: "8.0.5"`, `channel: "dev"` instead of `"8.0.5-dev"`.
- Lets analyzer reason about "is the user pinning a non-stable
  version" without parsing.
- Backwards-compat: keep emitting the combined string in a
  `display_version` field for legacy clients.

### 5. Latest pointers

`/v1/libraries/<name>/latest` → `{stable: …, dev: …, testing: …}`.
Lets the analyzer offer "update to latest" code actions and warn
when a project pin is N versions behind. Resolve in one hop instead
of fetching the whole versions list.

### 6. Integrity + provenance

Supply-chain hygiene is a real concern when the LSP / CLI installs
libs on behalf of the user:

- **sha256 per artifact**, returned in the version row.
- **Signed manifest** — the registry signs the per-version metadata
  (Sigstore / minisign). The CLI verifies before extracting.
- **Provenance attestation** — link to the build that produced the
  artifact (SLSA-style). Optional but valuable for first-party libs.
- **Yank + revocation** — clients re-check periodically; a yanked
  version emits a warning even when locally cached.

### 7. Dependency graph + resolution

Once `requires` is populated, the registry should support transitive
resolution server-side:

- `POST /v1/resolve` with `{deps: [{name, version_constraint}, …],
  runtime}` → fully-resolved lockfile (`{name, version, integrity,
  url}` per node).
- One round trip instead of N recursive client-side fetches; lets the
  analyzer compute "what `greycat install` would install" without
  network heroics.
- Output is the same shape as a `lib/installed.lock` so the CLI just
  writes it.

### 8. Search + facets

For `@library("<cursor>"…)` name completion to feel responsive when
the registry grows past a few dozen libs:

- `GET /v1/libraries?q=<query>&channel=stable&tag=ml&limit=50`.
- Server-side ranking by relevance + popularity (download counts).
- Returns the same shape as `/v1/libraries` so the same client code
  paths handle both.

### 9. Rate / freshness + caching headers

- **ETag / If-None-Match** on every endpoint. The LSP cache hits will
  be `304 Not Modified` — bandwidth-free freshness check.
- **Cache-Control: max-age=N, stale-while-revalidate=M** so editors
  can show a fresh list immediately and revalidate in the background.
- **A monotonically increasing `registry_version` field** in
  responses so clients know when the cache is older than the latest
  publication and can skip TTL.

### 10. WASM / playground friendliness

The playground runs in the browser; the LSP can't help it. Same
shape, same JSON, served over CORS so a browser `fetch()` can hit
the registry directly without a proxy. Strict CORS is fine; what
matters is that the endpoint is callable from `monaco-editor` running
in `claude.ai/code` or anywhere else.

### 11. Stable, versioned API

`/v1/...` prefix from day one. The current "list a directory" shape
isn't an API contract — it's the bare-metal output of the file server,
which is exactly why a rewrite is on the table. The new endpoints
should be a contract the analyzer can pin and the registry can evolve
without breaking N-year-old GreyCat installations.

### 12. CLI / tooling endpoints (nice-to-have)

Once the analyzer is happy, a few low-effort additions unlock CLI UX:

- `GET /v1/libraries/<name>` → full metadata: maintainers, links,
  README markdown.
- `GET /v1/libraries/<name>/<version>/changelog` → markdown.
- `GET /v1/libraries/<name>/<version>/readme` → markdown for
  `greycat info <name>@<version>` and rich hover.
- Download counts (`GET /v1/libraries/<name>/stats`) — for the
  "popular libraries" rail in the playground / web docs.

## What this changes for the analyzer

With (1)–(7) in place:

- **`@library("<cursor>", ...)`** name completion ships, surfacing
  every published lib with description / tags.
- **`@library("foo", "<cursor>")`** version completion drops from
  ~200 fetches per request to one, with full per-version metadata
  rendered in `labelDetails` + `documentation` without follow-ups.
- **Project diagnostics** can flag yanked versions, pin drift,
  runtime mismatch, and missing-arch warnings without speculative
  fetches.
- **Code actions** like "update to latest stable", "update to latest
  in this channel", "lock dependencies" become one-shot operations.
- **`greycat install`** can verify integrity + signatures + resolve
  transitive deps server-side, replacing the current ad-hoc walk.

## Open questions for the registry rewrite

1. **Auth model.** Is there a future where libraries are gated (org-
   private)? If so, the API should accept a bearer token from day one
   and the LSP needs an injection point (env var? `lib/installed`
   credentials? IDE secret?) to forward it.
2. **Mirror / offline support.** Should the registry advertise mirror
   URLs in responses so a CI box behind a firewall can pin a mirror
   without forking the analyzer config?
3. **Pre-release semantics.** Does `dev` become the implicit channel
   when no suffix is typed, or is `stable` still the default? Worth
   pinning before the new schema is locked.
4. **Compatibility with existing `@library("std", "x.y.z-stable")`
   pins.** The analyzer parses the suffix today; if `channel` becomes
   a separate field, both shapes will need to coexist for at least
   one release.
