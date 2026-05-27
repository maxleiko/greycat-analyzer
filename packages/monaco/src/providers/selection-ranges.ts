// Monaco `SelectionRangeProvider` backed by `Project::selectionRanges`.
//
// The IDE shape is a flat array of nested ranges, leaf-to-root. Monaco
// expects each request position to map to a linked SelectionRange
// chain (parent pointers). We rebuild the chain from the flat array.

import type * as MonacoNs from "monaco-editor";
import type { Project, Range as IdeRange } from "@greycat/analyzer";

export function registerSelectionRanges(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerSelectionRangeProvider(languageId, {
    provideSelectionRanges(model, positions) {
      return positions.map((pos) => {
        const ranges = project.selectionRanges(
          model.uri.toString(),
          pos.lineNumber - 1,
          pos.column - 1,
        );
        return ranges.map((r) => ({ range: toIRange(r) }));
      });
    },
  });
}

function toIRange(r: IdeRange): MonacoNs.IRange {
  return {
    startLineNumber: r.start.line + 1,
    startColumn: r.start.character + 1,
    endLineNumber: r.end.line + 1,
    endColumn: r.end.character + 1,
  };
}
