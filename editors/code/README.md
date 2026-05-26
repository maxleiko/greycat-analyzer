# GreyCat for Visual Studio Code

Language support for [GreyCat](https://greycat.io) (`.gcl`) — completion, diagnostics, formatting, hover, goto-definition, rename, code actions. Backed by [`greycat-analyzer`](https://github.com/maxleiko/greycat-analyzer), which doubles as a CLI linter and formatter for CI.

## Analyzer binary

The extension talks to the `greycat-analyzer` binary over LSP. On first launch in a `.gcl` workspace, if the binary isn't already installed, the extension offers to **download** the latest release from GitHub for your platform. The download is per-user and lives in VS Code's extension storage — no shell `PATH` shenanigans needed.

You can also use a binary you installed yourself: put it on `PATH` (`greycat-analyzer --version` should work) or point `greycat.serverPath` at an absolute path. Discovery order: `greycat.serverPath` → `PATH` → managed download → first-run prompt. First hit wins.

The extension periodically checks GitHub for newer releases (daily by default, configurable). When a newer release is available, it shows a notification; updates are never silent. Use `GreyCat: Check for Analyzer Updates` to force a check, or `GreyCat: Download / Update LSP Server` to install the latest right now.

Pre-built binaries exist for Linux x86_64 (glibc / musl), Apple Silicon macOS, and Windows x86_64. Intel Mac (`darwin/x64`) has no native artifact yet — install manually per the [project README](https://github.com/maxleiko/greycat-analyzer#install).

## Activation

Auto-activates when the workspace contains a `project.gcl`. The entrypoint's `@library` / `@include` pragmas define the analyzed module set — there is no flat directory walk, so projects nested in subfolders work as long as each has its own `project.gcl`.

## Settings

| Setting | Default | What it does |
| --- | --- | --- |
| `greycat.trace.server` | `info` | LSP server log verbosity. `off` / `info` / `debug` / `trace`. Output goes to the **GreyCat** output channel. |
| `greycat.lintLibs` | `false` | Surface lint warnings for vendored modules under `lib/<name>/`. Off by default so your own code's signal doesn't drown in stdlib noise. Type-relation diagnostics always surface. |
| `greycat.diagnosticsDebounceMs` | `150` | Debounce window (ms) between full analyzer publishes while you type. `0` runs the analyzer on every keystroke. |
| `greycat.serverPath` | `""` | Absolute path to a `greycat-analyzer` binary. When set, overrides every other discovery step. |
| `greycat.checkForUpdates` | `daily` | How often to probe GitHub for newer releases. `off` / `onStartup` / `daily` / `weekly`. |

Changes to these settings prompt you to restart the server.

## Commands

| Command | Action |
| --- | --- |
| `GreyCat: Restart LSP Server` | Stop and re-spawn the LSP server. |
| `GreyCat: Download / Update LSP Server` | Download the latest analyzer release into the extension's managed storage and restart. |
| `GreyCat: Show LSP Server Path` | Log the resolved binary path, its source (`settings` / `PATH` / `managed`), its `--version` output, and the latest known release tag. |
| `GreyCat: Check for LSP Server updates` | Force an update probe outside the auto-check cadence. |

## Troubleshooting

**`greycat-analyzer` not found and the download prompt didn't appear.** Try `GreyCat: Download / Update LSP Server` from the command palette directly. If that fails, install manually via the [project README](https://github.com/maxleiko/greycat-analyzer#install) and either restart VS Code (so it picks up the updated `PATH`) or set `greycat.serverPath` to the binary's absolute path.

**Suggestions not appearing on macOS.** `Ctrl+Space` may be claimed by the system "Select previous input source" shortcut. Disable it under **System Settings → Keyboard → Keyboard Shortcuts → Input Sources**, then restart VS Code.

**Stale diagnostics or hover.** Run `GreyCat: Restart LSP Server` from the command palette. If the issue reproduces from a clean restart, please [file an issue](https://github.com/maxleiko/greycat-analyzer/issues/new) with a minimal repro.

**Skipped a version notification and want it back.** Re-run `GreyCat: Check for LSP Server updates` — the manual command bypasses the "skip this version" gate.

## Bugs and feedback

Issues for the extension, the analyzer, the formatter, and the LSP server all live in the same place: [github.com/maxleiko/greycat-analyzer/issues](https://github.com/maxleiko/greycat-analyzer/issues).

## License

MIT.
