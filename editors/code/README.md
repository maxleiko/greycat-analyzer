# GreyCat for Visual Studio Code

Language support for [GreyCat](https://greycat.io) (`.gcl`) — completion, diagnostics, formatting, hover, goto-definition, rename, code actions. Backed by [`greycat-analyzer`](https://github.com/maxleiko/greycat-analyzer), which doubles as a CLI linter and formatter for CI.

## Requirements

The extension talks to the `greycat-analyzer` binary over LSP — it does **not** ship one. Install it first and make sure it's on your `PATH`:

```sh
greycat-analyzer --version
```

Install instructions for every platform are in the [project README](https://github.com/maxleiko/greycat-analyzer#install).

## Activation

Auto-activates when the workspace contains a `project.gcl`. The entrypoint's `@library` / `@include` pragmas define the analyzed module set — there is no flat directory walk, so projects nested in subfolders work as long as each has its own `project.gcl`.

## Settings

| Setting | Default | What it does |
| --- | --- | --- |
| `greycat.trace.server` | `info` | LSP server log verbosity. `off` / `info` / `debug` / `trace`. Output goes to the **GreyCat** output channel. |
| `greycat.lintLibs` | `false` | Surface lint warnings for vendored modules under `lib/<name>/`. Off by default so your own code's signal doesn't drown in stdlib noise. Type-relation diagnostics always surface. |
| `greycat.diagnosticsDebounceMs` | `150` | Debounce window (ms) between full analyzer publishes while you type. `0` runs the analyzer on every keystroke. |

Changes to these settings prompt you to restart the server.

## Commands

| Command | Action |
| --- | --- |
| `GreyCat: Restart LSP Server` | Stop and re-spawn the LSP server. |

## Troubleshooting

**`greycat-analyzer` not found.** The binary isn't on the shell `PATH` VS Code inherits. Reinstall via the [project README](https://github.com/maxleiko/greycat-analyzer#install) and restart VS Code (so it picks up the updated `PATH`).

**Suggestions not appearing on macOS.** `Ctrl+Space` may be claimed by the system "Select previous input source" shortcut. Disable it under **System Settings → Keyboard → Keyboard Shortcuts → Input Sources**, then restart VS Code.

**Stale diagnostics or hover.** Run `GreyCat: Restart LSP Server` from the command palette. If the issue reproduces from a clean restart, please [file an issue](https://github.com/maxleiko/greycat-analyzer/issues/new) with a minimal repro.

## Bugs and feedback

Issues for the extension, the analyzer, the formatter, and the LSP server all live in the same place: [github.com/maxleiko/greycat-analyzer/issues](https://github.com/maxleiko/greycat-analyzer/issues).

## License

MIT.
