// Monaco `DocumentSemanticTokensProvider` backed by
// `Project::semanticTokens`. The analyzer emits LSP-shape
// delta-encoded quintuples (delta_line, delta_start, length,
// token_type, token_modifiers) which Monaco consumes verbatim.
//
// Legend mirrors `SEMANTIC_TOKEN_TYPES` in
// greycat-analyzer-analysis/src/ide/semantic_tokens.rs. If the
// analyzer adds a new type id, mirror it here in the same slot —
// the legend's slot index is the token's type id.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";

export const SEMANTIC_TOKEN_TYPES = [
  "function",
  "type",
  "enum",
  "enumMember",
  "variable",
  "parameter",
  "string",
  "number",
  "comment",
  "keyword",
] as const;

export const SEMANTIC_TOKEN_MODIFIERS: readonly string[] = [];

export function registerSemanticTokens(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerDocumentSemanticTokensProvider(languageId, {
    getLegend() {
      return {
        tokenTypes: [...SEMANTIC_TOKEN_TYPES],
        tokenModifiers: [...SEMANTIC_TOKEN_MODIFIERS],
      };
    },
    provideDocumentSemanticTokens(model) {
      const tokens = project.semanticTokens(model.uri.toString());
      return { data: tokens.data };
    },
    releaseDocumentSemanticTokens() {},
  });
}
