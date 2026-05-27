// Monaco `DocumentSymbolProvider` backed by `Project::documentSymbols`.

import type * as MonacoNs from "monaco-editor";
import type { Project, DocumentSymbol as IdeSymbol } from "@greycat/analyzer";
import { SymbolKind } from "@greycat/analyzer";

export function registerDocumentSymbols(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerDocumentSymbolProvider(languageId, {
    displayName: "GreyCat",
    provideDocumentSymbols(model) {
      const symbols = project.documentSymbols(model.uri.toString());
      return symbols.map((s) => toMonaco(monaco, s));
    },
  });
}

function toMonaco(monaco: typeof MonacoNs, s: IdeSymbol): MonacoNs.languages.DocumentSymbol {
  return {
    name: s.name,
    detail: "",
    kind: kindToMonaco(monaco, s.kind),
    tags: [],
    range: {
      startLineNumber: s.range.start.line + 1,
      startColumn: s.range.start.character + 1,
      endLineNumber: s.range.end.line + 1,
      endColumn: s.range.end.character + 1,
    },
    selectionRange: {
      startLineNumber: s.selection_range.start.line + 1,
      startColumn: s.selection_range.start.character + 1,
      endLineNumber: s.selection_range.end.line + 1,
      endColumn: s.selection_range.end.character + 1,
    },
    children: s.children.map((c) => toMonaco(monaco, c)),
  };
}

function kindToMonaco(monaco: typeof MonacoNs, kind: SymbolKind): MonacoNs.languages.SymbolKind {
  switch (kind) {
    case SymbolKind.Function:
      return monaco.languages.SymbolKind.Function;
    case SymbolKind.Class:
      return monaco.languages.SymbolKind.Class;
    case SymbolKind.Enum:
      return monaco.languages.SymbolKind.Enum;
    case SymbolKind.Variable:
      return monaco.languages.SymbolKind.Variable;
    case SymbolKind.Key:
      return monaco.languages.SymbolKind.Key;
    case SymbolKind.Field:
      return monaco.languages.SymbolKind.Field;
    case SymbolKind.Method:
      return monaco.languages.SymbolKind.Method;
    default: {
      const _exhaustive: never = kind;
      return _exhaustive;
    }
  }
}
