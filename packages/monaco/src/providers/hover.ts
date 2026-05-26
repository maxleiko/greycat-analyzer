// Monaco `HoverProvider` backed by `Project::hover`. The IDE-shape
// `Hover { range, markdown }` maps cleanly to Monaco's
// `Hover { contents, range }` with a single-element markdown
// contents array.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";

export function registerHover(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerHoverProvider(languageId, {
    provideHover(model, position) {
      const uri = model.uri.toString();
      const hover = project.hover(uri, position.lineNumber - 1, position.column - 1);
      if (!hover) {
        return null;
      }
      return {
        contents: [{ value: hover.markdown, isTrusted: true }],
        range: {
          startLineNumber: hover.range.start.line + 1,
          startColumn: hover.range.start.character + 1,
          endLineNumber: hover.range.end.line + 1,
          endColumn: hover.range.end.character + 1,
        },
      };
    },
  });
}
