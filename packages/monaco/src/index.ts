// Monaco language providers for GreyCat.
//
// Top-level entry: `registerGreycat(monaco, project)` registers the
// `"greycat"` language id and every supported provider (completion,
// hover, signature help, inlay hints, code actions, references,
// rename, document symbols, folding ranges, selection ranges,
// document highlights, formatting, semantic tokens) plus the push-
// driven diagnostics wire-up against a Monaco namespace and a
// `Project` handle.
//
// Each provider lives in its own file under `./providers/` so adding
// or replacing one is a one-file change. This entry just wires them
// up and returns a `dispose()` that tears everything down.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";

import { LANGUAGE_CONFIGURATION, MONARCH_LANGUAGE } from "./monarch.js";
import { registerCodeActions } from "./providers/code-actions.js";
import { registerCompletion } from "./providers/completion.js";
import { attachDiagnostics } from "./providers/diagnostics.js";
import { registerDocumentHighlights } from "./providers/document-highlights.js";
import { registerDocumentSymbols } from "./providers/document-symbols.js";
import { registerFoldingRanges } from "./providers/folding-ranges.js";
import { registerFormatting } from "./providers/formatting.js";
import { registerHover } from "./providers/hover.js";
import { registerInlayHints } from "./providers/inlay-hints.js";
import { registerReferences } from "./providers/references.js";
import { registerRename } from "./providers/rename.js";
import { registerSelectionRanges } from "./providers/selection-ranges.js";
import { registerSemanticTokens } from "./providers/semantic-tokens.js";
import { registerSignatureHelp } from "./providers/signature-help.js";

export const LANGUAGE_ID = "greycat";

export interface Registration {
  /** Dispose every registered provider + the diagnostics wire-up.
   *  Call on hot-reload to avoid duplicate registrations leaking. */
  dispose(): void;
}

export function registerGreycat(monaco: typeof MonacoNs, project: Project): Registration {
  monaco.languages.register({
    id: LANGUAGE_ID,
    extensions: [".gcl"],
    aliases: ["GreyCat", "greycat", "gcl"],
  });
  monaco.languages.setMonarchTokensProvider(LANGUAGE_ID, MONARCH_LANGUAGE);
  monaco.languages.setLanguageConfiguration(LANGUAGE_ID, LANGUAGE_CONFIGURATION);

  const disposables: MonacoNs.IDisposable[] = [
    registerCompletion(monaco, project, LANGUAGE_ID),
    registerHover(monaco, project, LANGUAGE_ID),
    registerSignatureHelp(monaco, project, LANGUAGE_ID),
    registerInlayHints(monaco, project, LANGUAGE_ID),
    registerCodeActions(monaco, project, LANGUAGE_ID),
    registerReferences(monaco, project, LANGUAGE_ID),
    registerRename(monaco, project, LANGUAGE_ID),
    registerDocumentSymbols(monaco, project, LANGUAGE_ID),
    registerFoldingRanges(monaco, project, LANGUAGE_ID),
    registerSelectionRanges(monaco, project, LANGUAGE_ID),
    registerDocumentHighlights(monaco, project, LANGUAGE_ID),
    registerFormatting(monaco, project, LANGUAGE_ID),
    registerSemanticTokens(monaco, project, LANGUAGE_ID),
    attachDiagnostics(monaco, project, LANGUAGE_ID),
  ];

  return {
    dispose() {
      for (const d of disposables) {
        d.dispose();
      }
    },
  };
}

export { SEMANTIC_TOKEN_TYPES, SEMANTIC_TOKEN_MODIFIERS } from "./providers/semantic-tokens.js";
