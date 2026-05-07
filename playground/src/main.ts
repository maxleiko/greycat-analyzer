// Playground entry point. Sets up WebAwesome, mounts Monaco directly
// onto a plain DOM node (Monaco is incompatible with shadow DOM —
// it injects styles into document.head and queries computed styles
// on its host), and broadcasts source changes to each Lit panel.
//
// The layout itself lives in index.html, NOT in a Lit root component.
// WebAwesome's auto-loader walks `document` to register `<wa-*>`
// elements, and MutationObserver does not cross shadow boundaries —
// putting the layout inside a Lit shadow root prevents `<wa-tab-group>`
// & friends from ever being upgraded.
//
// We import each WA component statically rather than relying on the
// runtime auto-loader: the auto-loader expects component modules to be
// fetchable at `/components/<name>/<name>.js` (configurable via
// setBasePath). Static imports let Vite bundle them and skip the
// runtime fetch dance entirely.

import "@awesome.me/webawesome/dist/styles/webawesome.css";
import "@awesome.me/webawesome/dist/components/split-panel/split-panel.js";
import "@awesome.me/webawesome/dist/components/tab-group/tab-group.js";
import "@awesome.me/webawesome/dist/components/tab/tab.js";
import "@awesome.me/webawesome/dist/components/tab-panel/tab-panel.js";

import "./style.css";

// Panels (Lit elements; their own shadow DOM is fine — they're leaves).
import "./components/gc-cst-panel.ts";
import "./components/gc-hir-panel.ts";
import "./components/gc-tokens-panel.ts";
import "./components/gc-types-panel.ts";
import "./components/gc-diagnostics-panel.ts";
import "./components/gc-format-panel.ts";

import * as monaco from "monaco-editor";
import editorWorkerUrl from "monaco-editor/esm/vs/editor/editor.worker?worker&url";

import { SAMPLE_SOURCE } from "./sample.ts";
import { registerGcl } from "./gcl-language.ts";

self.MonacoEnvironment = {
  getWorkerUrl() {
    return editorWorkerUrl;
  },
};

const PANEL_TAGS = [
  "gc-diagnostics-panel",
  "gc-cst-panel",
  "gc-tokens-panel",
  "gc-hir-panel",
  "gc-types-panel",
  "gc-format-panel",
] as const;

function broadcastSource(src: string) {
  const bytes = document.getElementById("bytes");
  if (bytes) bytes.textContent = `${src.length} bytes`;
  for (const tag of PANEL_TAGS) {
    document.querySelectorAll(tag).forEach((el) => {
      (el as HTMLElement & { source?: string }).source = src;
    });
  }
}

function mountEditor() {
  const host = document.getElementById("editor-host");
  if (!host) throw new Error("missing #editor-host");
  registerGcl();
  const editor = monaco.editor.create(host, {
    value: SAMPLE_SOURCE,
    language: "gcl",
    theme: matchMedia("(prefers-color-scheme: dark)").matches ? "vs-dark" : "vs",
    automaticLayout: true,
    minimap: { enabled: false },
    fontFamily: "JetBrains Mono, Fira Code, ui-monospace, monospace",
    fontSize: 13,
    tabSize: 4,
    insertSpaces: true,
    scrollBeyondLastLine: false,
  });

  editor.onDidChangeModelContent(() => {
    broadcastSource(editor.getValue());
  });

  broadcastSource(SAMPLE_SOURCE);
}

mountEditor();

declare global {
  interface Window {
    MonacoEnvironment?: {
      getWorkerUrl?(moduleId: string, label: string): string;
      getWorker?(moduleId: string, label: string): Worker;
    };
  }
}
