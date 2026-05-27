// Monaco `InlayHintsProvider` backed by `Project::inlayHints`.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";
import { InlayHintKind } from "@greycat/analyzer";

export function registerInlayHints(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerInlayHintsProvider(languageId, {
    provideInlayHints(model, range) {
      const hints = project.inlayHints(
        model.uri.toString(),
        range.startLineNumber - 1,
        range.startColumn - 1,
        range.endLineNumber - 1,
        range.endColumn - 1,
      );
      return {
        hints: hints.map((h) => ({
          label: h.label,
          position: {
            lineNumber: h.position.line + 1,
            column: h.position.character + 1,
          },
          kind: kindToMonaco(monaco, h.kind),
          paddingLeft: h.padding_left,
          paddingRight: h.padding_right,
        })),
        dispose() {},
      };
    },
  });
}

function kindToMonaco(
  monaco: typeof MonacoNs,
  kind: InlayHintKind,
): MonacoNs.languages.InlayHintKind {
  switch (kind) {
    case InlayHintKind.Type:
      return monaco.languages.InlayHintKind.Type;
    case InlayHintKind.Parameter:
      return monaco.languages.InlayHintKind.Parameter;
    default: {
      const _exhaustive: never = kind;
      return _exhaustive;
    }
  }
}
