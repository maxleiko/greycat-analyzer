// Monaco `DocumentHighlightProvider` backed by
// `Project::documentHighlights`.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";
import { DocumentHighlightKind } from "@greycat/analyzer";

export function registerDocumentHighlights(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerDocumentHighlightProvider(languageId, {
    provideDocumentHighlights(model, position) {
      const highlights = project.documentHighlights(
        model.uri.toString(),
        position.lineNumber - 1,
        position.column - 1,
      );
      return highlights.map((h) => ({
        range: {
          startLineNumber: h.range.start.line + 1,
          startColumn: h.range.start.character + 1,
          endLineNumber: h.range.end.line + 1,
          endColumn: h.range.end.character + 1,
        },
        kind: kindToMonaco(monaco, h.kind),
      }));
    },
  });
}

function kindToMonaco(
  monaco: typeof MonacoNs,
  kind: DocumentHighlightKind,
): MonacoNs.languages.DocumentHighlightKind {
  switch (kind) {
    case DocumentHighlightKind.Text:
      return monaco.languages.DocumentHighlightKind.Text;
    case DocumentHighlightKind.Read:
      return monaco.languages.DocumentHighlightKind.Read;
    case DocumentHighlightKind.Write:
      return monaco.languages.DocumentHighlightKind.Write;
    default: {
      const _exhaustive: never = kind;
      return _exhaustive;
    }
  }
}
