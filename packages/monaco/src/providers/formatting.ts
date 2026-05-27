// Monaco `DocumentFormattingEditProvider` backed by `Project::format`.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";

export function registerFormatting(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerDocumentFormattingEditProvider(languageId, {
    displayName: "GreyCat",
    provideDocumentFormattingEdits(model) {
      const edits = project.format(model.uri.toString());
      return edits.map((edit) => ({
        range: {
          startLineNumber: edit.range.start.line + 1,
          startColumn: edit.range.start.character + 1,
          endLineNumber: edit.range.end.line + 1,
          endColumn: edit.range.end.character + 1,
        },
        text: edit.new_text,
      }));
    },
  });
}
