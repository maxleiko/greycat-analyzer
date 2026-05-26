// Monaco language providers for GreyCat.
//
// Top-level entry: `registerGreycat(monaco, project, options?)` registers
// the language id + every supported provider (completion, hover,
// signature help, inlay hints, code actions, references, rename,
// document symbols, folding ranges, selection ranges, document
// highlights, formatting, semantic tokens, diagnostics) against a
// `Monaco` namespace + a wasm `Project` handle.
//
// Each provider lives in its own file under `./providers/` so adding
// or replacing one is a one-file change. This entry just wires them up.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";

import { registerCompletion } from "./providers/completion.js";
import { registerHover } from "./providers/hover.js";

export interface RegisterOptions {
  /** Language id to use when registering with Monaco. Default `"greycat"`. */
  languageId?: string;
}

export interface Registration {
  /** Dispose every registered provider. Call this on hot-reload to
   *  avoid duplicate registrations leaking. */
  dispose(): void;
}

/** Register the GreyCat language + every provider against `monaco`. */
export function registerGreycat(
  monaco: typeof MonacoNs,
  project: Project,
  options: RegisterOptions = {},
): Registration {
  const languageId = options.languageId ?? "greycat";

  // Register the language id once. Idempotent — Monaco silently
  // accepts the second call if the id is already known.
  monaco.languages.register({
    id: languageId,
    extensions: [".gcl"],
    aliases: ["GreyCat", "greycat", "GCL"],
  });

  const disposables: MonacoNs.IDisposable[] = [
    registerCompletion(monaco, project, languageId),
    registerHover(monaco, project, languageId),
  ];

  return {
    dispose() {
      for (const d of disposables) {
        d.dispose();
      }
    },
  };
}
