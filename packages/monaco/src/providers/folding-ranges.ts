// Monaco `FoldingRangeProvider` backed by `Project::foldingRanges`.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";
import { FoldingRangeKind } from "@greycat/analyzer";

export function registerFoldingRanges(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerFoldingRangeProvider(languageId, {
    provideFoldingRanges(model) {
      const ranges = project.foldingRanges(model.uri.toString());
      return ranges.map((r) => ({
        start: r.start_line + 1,
        end: r.end_line + 1,
        kind: kindToMonaco(monaco, r.kind),
      }));
    },
  });
}

function kindToMonaco(
  monaco: typeof MonacoNs,
  kind: FoldingRangeKind,
): MonacoNs.languages.FoldingRangeKind | undefined {
  switch (kind) {
    case FoldingRangeKind.Comment:
      return monaco.languages.FoldingRangeKind.Comment;
    case FoldingRangeKind.Imports:
      return monaco.languages.FoldingRangeKind.Imports;
    case FoldingRangeKind.Region:
      return monaco.languages.FoldingRangeKind.Region;
    default: {
      const _exhaustive: never = kind;
      return _exhaustive;
    }
  }
}
