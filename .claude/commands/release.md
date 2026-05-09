---
description: Cut a new release of greycat-analyzer (bump version, tag, push — triggers GitHub Actions release workflow)
---

# /release

Cuts a release of `greycat-analyzer` by bumping the workspace version, tagging the commit, and pushing both. The push of the `v*` tag triggers [`.github/workflows/release.yml`](../../.github/workflows/release.yml), which builds binaries for Linux x86_64, macOS x86_64, macOS arm64, and Windows x86_64, then drafts a GitHub Release with the archives + `SHA256SUMS` attached.

The release is created **in draft mode** so it's reviewable before going live. After CI finishes, promote the draft to a published release from the GitHub UI.

## Arguments

- `$1` (optional) — the new version string, e.g. `0.2.0` or `0.2.0-rc1`. **No `v` prefix** here; the prefix is added when tagging.
- If omitted, ask the user what version they want and read the current version from `Cargo.toml` `[workspace.package]` to give context (suggest the next patch / minor / major).

## Pre-flight checklist

Before bumping anything, confirm all of these. Stop and report if any fails:

1. **Working tree is clean** — `git status --porcelain` returns nothing. If there are uncommitted changes, ask the user to stash / commit / discard before continuing.
2. **On `main`** — `git rev-parse --abbrev-ref HEAD` is `main`. (If they want to release from another branch, ask first — non-`main` releases are unusual.)
3. **`main` is up to date** — `git fetch origin main` then `git rev-list HEAD..origin/main --count` is `0`. If not, the user needs to pull / rebase first.
4. **CI gauntlet is green locally** — run in this order, abort on any failure:
   - `cargo fmt --all -- --check`
   - `cargo build --workspace`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo test --workspace`
5. **Tag does not already exist** — `git rev-parse "v$VERSION" 2>/dev/null` returns nothing. If the tag exists already, abort and ask the user if they meant a different version.

## The bump

1. Read the current version from the root `Cargo.toml` (under `[workspace.package]`, the `version = "X.Y.Z"` line).
2. Edit that line to the new version. **Only edit the `[workspace.package]` `version`** — every workspace crate inherits via `version.workspace = true`.
3. Run `cargo build --workspace` to refresh `Cargo.lock` with the new version.
4. Show the user a `git diff` covering `Cargo.toml` + `Cargo.lock` so they can sanity-check before the commit.

## Commit, tag, push

Once the user confirms the diff:

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore(release): v$VERSION"
git tag -a "v$VERSION" -m "v$VERSION"
git push origin main
git push origin "v$VERSION"
```

The tag push is what triggers the GitHub Actions workflow.

## After the push

1. Surface the workflow URL to the user: `https://github.com/maxleiko/greycat-analyzer/actions/workflows/release.yml`.
2. Tell them the workflow takes ~5–10 minutes (4 build jobs + a release job) and will create a **draft** release at `https://github.com/maxleiko/greycat-analyzer/releases`.
3. Once the draft is up, the user reviews the artifacts (tar.gz / zip per platform + `SHA256SUMS`) and promotes the draft to a published release manually.

## Recovery: pushed the wrong tag

If the tag is wrong but the workflow hasn't started a release yet:

```bash
git tag -d "v$VERSION"
git push origin --delete "v$VERSION"
```

Then re-run `/release` with the right version. **Do NOT** force-push tags after a release has been published — downstream installers may have cached the artifacts.

## Recovery: workflow failed mid-flight

The workflow supports `workflow_dispatch` with a `tag` input. From the Actions tab, manually re-run the `Release` workflow against the existing tag. No re-tagging needed.

## Notes on cadence

- Patch versions (`0.X.Y` → `0.X.Y+1`) for bug fixes, no API changes.
- Minor versions (`0.X.Y` → `0.X+1.0`) for new features / additive changes (we're pre-1.0 so this is the dominant shape).
- Major versions (`0.X.Y` → `1.0.0` and onward) when API stability is in scope. Don't bump to `1.0` without explicit user direction.
- Pre-release suffixes (`-rc1`, `-beta`, `-dev`) are fine and supported by the workflow trigger pattern (`v*`).
