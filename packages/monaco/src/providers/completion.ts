// Monaco `CompletionItemProvider` backed by `Project::completion`.
// Maps the IDE-shape `CompletionList` from `@greycat/analyzer` to
// Monaco's `CompletionList`. The IDE `CompletionItemKind` enum is
// a strict subset of Monaco's, so the per-kind mapping is total.

import type * as MonacoNs from "monaco-editor";
import type { Project, CompletionItem as IdeCompletionItem } from "@greycat/analyzer";
import { CompletionItemKind } from "@greycat/analyzer";

export function registerCompletion(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerCompletionItemProvider(languageId, {
    triggerCharacters: [".", ":", "@", '"'],
    provideCompletionItems(model, position) {
      const uri = model.uri.toString();
      const list = project.completion(uri, position.lineNumber - 1, position.column - 1);
      if (!list) {
        return { suggestions: [] };
      }
      return {
        incomplete: list.is_incomplete,
        suggestions: list.items.map((item) => toMonaco(monaco, item, position)),
      };
    },
  });
}

function toMonaco(
  monaco: typeof MonacoNs,
  item: IdeCompletionItem,
  position: MonacoNs.Position,
): MonacoNs.languages.CompletionItem {
  const range: MonacoNs.IRange = item.text_edit
    ? {
        startLineNumber: item.text_edit.range.start.line + 1,
        startColumn: item.text_edit.range.start.character + 1,
        endLineNumber: item.text_edit.range.end.line + 1,
        endColumn: item.text_edit.range.end.character + 1,
      }
    : {
        startLineNumber: position.lineNumber,
        startColumn: position.column,
        endLineNumber: position.lineNumber,
        endColumn: position.column,
      };
  return {
    label: item.label,
    kind: kindToMonaco(monaco, item.kind),
    insertText: item.text_edit?.new_text ?? item.insert_text ?? item.label,
    insertTextRules:
      item.insert_text_format === 1
        ? monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet
        : undefined,
    range,
    sortText: item.sort_text,
    filterText: item.filter_text,
    detail: item.detail,
    documentation: item.documentation ? { value: item.documentation, isTrusted: true } : undefined,
    additionalTextEdits: item.additional_text_edits?.map((edit) => ({
      range: {
        startLineNumber: edit.range.start.line + 1,
        startColumn: edit.range.start.character + 1,
        endLineNumber: edit.range.end.line + 1,
        endColumn: edit.range.end.character + 1,
      },
      text: edit.new_text,
    })),
  };
}

function kindToMonaco(
  monaco: typeof MonacoNs,
  kind: CompletionItemKind | undefined,
): MonacoNs.languages.CompletionItemKind {
  if (kind === undefined) {
    return monaco.languages.CompletionItemKind.Text;
  }
  switch (kind) {
    case CompletionItemKind.Function:
      return monaco.languages.CompletionItemKind.Function;
    case CompletionItemKind.Method:
      return monaco.languages.CompletionItemKind.Method;
    case CompletionItemKind.Variable:
      return monaco.languages.CompletionItemKind.Variable;
    case CompletionItemKind.Field:
      return monaco.languages.CompletionItemKind.Field;
    case CompletionItemKind.Class:
      return monaco.languages.CompletionItemKind.Class;
    case CompletionItemKind.Enum:
      return monaco.languages.CompletionItemKind.Enum;
    case CompletionItemKind.EnumMember:
      return monaco.languages.CompletionItemKind.EnumMember;
    case CompletionItemKind.Constant:
      return monaco.languages.CompletionItemKind.Constant;
    case CompletionItemKind.Module:
      return monaco.languages.CompletionItemKind.Module;
    case CompletionItemKind.Folder:
      return monaco.languages.CompletionItemKind.Folder;
    case CompletionItemKind.Keyword:
      return monaco.languages.CompletionItemKind.Keyword;
    case CompletionItemKind.Text:
      return monaco.languages.CompletionItemKind.Text;
    case CompletionItemKind.TypeParameter:
      return monaco.languages.CompletionItemKind.TypeParameter;
    default: {
      // Exhaustive — if a new variant lands in
      // `@greycat/analyzer`'s `CompletionItemKind` and isn't wired up
      // here, this branch fails the compile via the `never` check.
      const _exhaustive: never = kind;
      return _exhaustive;
    }
  }
}
