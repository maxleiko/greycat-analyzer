// Diagnostics is push-driven in Monaco (`setModelMarkers`), not
// provider-callback-based — so this module owns the model lifecycle
// for the registered language id:
//
//   - Walks `monaco.editor.getModels()` at registration time and
//     attaches a content-change listener to each existing matching
//     model.
//   - Subscribes to `monaco.editor.onDidCreateModel` so future models
//     get the same treatment.
//   - On every content change: `await project.didChange(uri)` (so the
//     analyzer's internal cache is current) and push fresh markers.
//   - On model dispose / language change: tear down the per-model
//     listener and clear markers under our owner key.
//
// Note: `didChange` IS the analyzer's primary edit signal. By owning
// it here we relieve callers from wiring it up — they construct the
// `Project`, call `registerGreycat`, and edits flow.

import type * as MonacoNs from "monaco-editor";
import type { Project, Diagnostic } from "@greycat/analyzer";
import { Severity, Tag } from "@greycat/analyzer";

const OWNER_PREFIX = "greycat";

export function attachDiagnostics(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  const owner = `${OWNER_PREFIX}:${languageId}`;
  // Keyed by `model.uri.toString()` — that's the stable cross-call
  // identifier; `IModel.id` is not part of the public API.
  const perModel = new Map<string, MonacoNs.IDisposable>();

  function refresh(model: MonacoNs.editor.ITextModel) {
    const uri = model.uri.toString();
    const diagnostics = project.diagnostics(uri);
    monaco.editor.setModelMarkers(
      model,
      owner,
      diagnostics.map((d) => toMarker(monaco, d)),
    );
  }

  function attach(model: MonacoNs.editor.ITextModel) {
    if (model.getLanguageId() !== languageId) {
      return;
    }
    const key = model.uri.toString();
    if (perModel.has(key)) {
      return;
    }
    const subs: MonacoNs.IDisposable[] = [];
    subs.push(
      model.onDidChangeContent(() => {
        void project
          .didChange(key)
          .then(() => refresh(model))
          .catch(() => refresh(model));
      }),
    );
    subs.push(
      model.onWillDispose(() => {
        teardown(key);
      }),
    );
    subs.push(
      model.onDidChangeLanguage((ev) => {
        if (ev.newLanguage !== languageId) {
          teardown(key);
        }
      }),
    );
    perModel.set(key, {
      dispose() {
        for (const s of subs) {
          s.dispose();
        }
      },
    });
    refresh(model);
  }

  function teardown(key: string) {
    const entry = perModel.get(key);
    if (!entry) {
      return;
    }
    entry.dispose();
    perModel.delete(key);
    const model = monaco.editor.getModel(monaco.Uri.parse(key));
    if (model) {
      monaco.editor.setModelMarkers(model, owner, []);
    }
  }

  for (const m of monaco.editor.getModels()) {
    attach(m);
  }
  const onCreate = monaco.editor.onDidCreateModel(attach);

  return {
    dispose() {
      onCreate.dispose();
      // Snapshot keys via Array.from — teardown() mutates the live map.
      const keys = Array.from(perModel.keys());
      for (const key of keys) {
        teardown(key);
      }
    },
  };
}

function toMarker(monaco: typeof MonacoNs, d: Diagnostic): MonacoNs.editor.IMarkerData {
  return {
    severity: severityToMarker(monaco, d.severity),
    message: d.message,
    code: d.code,
    source: d.source,
    startLineNumber: d.range.start.line + 1,
    startColumn: d.range.start.character + 1,
    endLineNumber: d.range.end.line + 1,
    endColumn: d.range.end.character + 1,
    tags: tagToMarker(monaco, d.tag),
  };
}

function severityToMarker(monaco: typeof MonacoNs, sev: Severity): MonacoNs.MarkerSeverity {
  switch (sev) {
    case Severity.Error:
      return monaco.MarkerSeverity.Error;
    case Severity.Warning:
      return monaco.MarkerSeverity.Warning;
    case Severity.Hint:
      return monaco.MarkerSeverity.Hint;
    default: {
      const _exhaustive: never = sev;
      return _exhaustive;
    }
  }
}

function tagToMarker(
  monaco: typeof MonacoNs,
  tag: Tag | undefined,
): MonacoNs.MarkerTag[] | undefined {
  if (tag === undefined) {
    return undefined;
  }
  switch (tag) {
    case Tag.Unnecessary:
      return [monaco.MarkerTag.Unnecessary];
    case Tag.Deprecated:
      return [monaco.MarkerTag.Deprecated];
    default: {
      const _exhaustive: never = tag;
      return _exhaustive;
    }
  }
}
