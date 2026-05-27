// Playground entry — a minimal app that validates `@greycat/monaco`
// against `@greycat/analyzer`. It mounts a Monaco editor, builds a
// `Project` from the editor buffer, calls `registerGreycat` so the
// editor gains hover + completion (and every other provider as they
// land in `@greycat/monaco`), and re-syncs the project on edits.
//
// No worker — the wasm analyzer runs on the main thread. That's fine
// for a smoke test; the worker entry exists in `@greycat/analyzer/worker`
// for consumers who need it.

import * as monaco from "monaco-editor";
import editorWorkerUrl from "monaco-editor/esm/vs/editor/editor.worker?worker&url";

import { Project, type LibraryResolver, type Context } from "@greycat/analyzer";
import { registerGreycat } from "@greycat/monaco";

import { SAMPLE_SOURCE } from "./sample.ts";
import "./style.css";

const ENTRYPOINT = "file:///main.gcl";

// The sample is fully self-contained (see `./sample.ts`), so the
// resolver never gets called. Throwing here documents the contract;
// once we add a real registry, swap this for `RegistryLibraryResolver`.
const noLibraries: LibraryResolver = {
  resolve(name, version) {
    throw new Error(
      `unexpected @library("${name}", "${version}") — playground sample is standalone`,
    );
  },
};

self.MonacoEnvironment = {
  getWorkerUrl() {
    return editorWorkerUrl;
  },
};

async function main() {
  const host = document.getElementById("editor-host");
  if (!host) {
    throw new Error("missing #editor-host");
  }

  // The model URI MUST match the Project entrypoint — every Monaco
  // provider in `@greycat/monaco` passes `model.uri.toString()` to
  // `project.hover/completion/semanticTokens/...`, so a mismatched
  // URI silently breaks every feature.
  const model = monaco.editor.createModel(
    SAMPLE_SOURCE,
    "greycat",
    monaco.Uri.parse(ENTRYPOINT),
  );

  const editor = monaco.editor.create(host, {
    model,
    theme: matchMedia("(prefers-color-scheme: dark)").matches ? "vs-dark" : "vs",
    automaticLayout: true,
    minimap: { enabled: false },
    fontFamily: "JetBrains Mono, Fira Code, ui-monospace, monospace",
    fontSize: 13,
    tabSize: 4,
    insertSpaces: true,
    scrollBeyondLastLine: false,
  });

  // The Context reads straight off Monaco's model so edits are
  // visible to the analyzer the moment they happen — no snapshot,
  // no sync step.
  const context: Context = {
    read(uri) {
      if (uri !== ENTRYPOINT) {
        return undefined;
      }
      return editor.getModel()?.getValue();
    },
    uris() {
      return [ENTRYPOINT];
    },
  };

  const project = await Project.create({
    entrypoint: ENTRYPOINT,
    context,
    libraries: noLibraries,
  });

  // registerGreycat owns the model-change → project.didChange loop
  // (see `attachDiagnostics` in @greycat/monaco), so main.ts doesn't
  // need to wire that up explicitly.
  registerGreycat(monaco, project);
}

void main();

declare global {
  interface Window {
    MonacoEnvironment?: {
      getWorkerUrl?(moduleId: string, label: string): string;
      getWorker?(moduleId: string, label: string): Worker;
    };
  }
}
