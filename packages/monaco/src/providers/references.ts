// Monaco `ReferenceProvider` backed by `Project::references`.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";

export function registerReferences(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerReferenceProvider(languageId, {
    provideReferences(model, position) {
      const locations = project.references(
        model.uri.toString(),
        position.lineNumber - 1,
        position.column - 1,
      );
      return locations.map((loc) => ({
        uri: monaco.Uri.parse(loc.uri),
        range: {
          startLineNumber: loc.range.start.line + 1,
          startColumn: loc.range.start.character + 1,
          endLineNumber: loc.range.end.line + 1,
          endColumn: loc.range.end.character + 1,
        },
      }));
    },
  });
}
