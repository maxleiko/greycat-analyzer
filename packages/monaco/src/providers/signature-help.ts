// Monaco `SignatureHelpProvider` backed by `Project::signatureHelp`.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";

export function registerSignatureHelp(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerSignatureHelpProvider(languageId, {
    signatureHelpTriggerCharacters: ["(", ","],
    signatureHelpRetriggerCharacters: [","],
    provideSignatureHelp(model, position) {
      const help = project.signatureHelp(
        model.uri.toString(),
        position.lineNumber - 1,
        position.column - 1,
      );
      if (!help) {
        return null;
      }
      return {
        value: {
          signatures: help.signatures.map((sig) => ({
            label: sig.label,
            documentation: sig.documentation
              ? { value: sig.documentation, isTrusted: true }
              : undefined,
            parameters: sig.parameters.map((p) => ({
              label: [p.label_start, p.label_end] as [number, number],
              documentation: p.documentation
                ? { value: p.documentation, isTrusted: true }
                : undefined,
            })),
            activeParameter: sig.active_parameter,
          })),
          activeSignature: help.active_signature,
          activeParameter: help.active_parameter,
        },
        dispose() {},
      };
    },
  });
}
