// Monaco `CodeActionProvider` backed by `Project::codeActions`.

import type * as MonacoNs from "monaco-editor";
import type { Project, CodeAction as IdeCodeAction } from "@greycat/analyzer";

export function registerCodeActions(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerCodeActionProvider(languageId, {
    provideCodeActions(model, range) {
      const actions = project.codeActions(
        model.uri.toString(),
        range.startLineNumber - 1,
        range.startColumn - 1,
        range.endLineNumber - 1,
        range.endColumn - 1,
      );
      return {
        actions: actions.map((a) => toMonacoAction(monaco, a)),
        dispose() {},
      };
    },
  });
}

function toMonacoAction(
  monaco: typeof MonacoNs,
  action: IdeCodeAction,
): MonacoNs.languages.CodeAction {
  return {
    title: action.title,
    kind: "quickfix",
    diagnostics: [diagnosticToMarker(monaco, action.diagnostic)],
    edit: {
      edits: action.edits.flatMap((perUri) =>
        perUri.edits.map((edit) => ({
          resource: monaco.Uri.parse(perUri.uri),
          textEdit: {
            range: {
              startLineNumber: edit.range.start.line + 1,
              startColumn: edit.range.start.character + 1,
              endLineNumber: edit.range.end.line + 1,
              endColumn: edit.range.end.character + 1,
            },
            text: edit.new_text,
          },
          versionId: undefined,
        })),
      ),
    },
    isPreferred: true,
  };
}

function diagnosticToMarker(
  monaco: typeof MonacoNs,
  d: IdeCodeAction["diagnostic"],
): MonacoNs.editor.IMarkerData {
  return {
    severity: monaco.MarkerSeverity.Error,
    message: d.message,
    code: d.code,
    source: d.source,
    startLineNumber: d.range.start.line + 1,
    startColumn: d.range.start.character + 1,
    endLineNumber: d.range.end.line + 1,
    endColumn: d.range.end.character + 1,
  };
}
