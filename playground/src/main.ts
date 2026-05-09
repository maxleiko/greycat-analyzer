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
import { getAnalyzer } from "./analyzer-client.ts";

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

// P14.7 — persist the editor buffer across page refreshes. Uses
// `localStorage` directly (no library). The key is namespaced so
// other apps on the same origin don't clobber it.
const STORAGE_KEY = "greycat-playground:source";
const STORAGE_DEBOUNCE_MS = 250;

function loadStoredSource(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (typeof stored === "string" && stored.length > 0) {
      return stored;
    }
  } catch {
    // Storage unavailable (private mode / disabled) — fall through.
  }
  return SAMPLE_SOURCE;
}

function persistSource(src: string) {
  try {
    localStorage.setItem(STORAGE_KEY, src);
  } catch {
    // Quota exceeded / disabled — ignore. The editor still works.
  }
}

function mountEditor() {
  const host = document.getElementById("editor-host");
  if (!host) throw new Error("missing #editor-host");
  registerGcl();
  const initial = loadStoredSource();
  const editor = monaco.editor.create(host, {
    value: initial,
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

  let persistTimer: ReturnType<typeof setTimeout> | null = null;
  editor.onDidChangeModelContent(() => {
    const value = editor.getValue();
    broadcastSource(value);
    if (persistTimer) clearTimeout(persistTimer);
    persistTimer = setTimeout(() => persistSource(value), STORAGE_DEBOUNCE_MS);
  });

  broadcastSource(initial);

  // P14.7 — click-to-jump: panels dispatch `gc-jump` (CustomEvent)
  // with `{ start, end }` byte offsets when a row is clicked. The
  // main.ts listener converts byte → Monaco position and selects the
  // range in the editor, scrolling it into view. Panels live in
  // shadow DOM (their own roots), so we listen at `document` — Lit
  // composed events bubble across boundaries.
  document.addEventListener("gc-jump", (ev: Event) => {
    const detail = (ev as CustomEvent<{ start: number; end: number }>).detail;
    if (!detail || typeof detail.start !== "number") return;
    const model = editor.getModel();
    if (!model) return;
    const startPos = model.getPositionAt(detail.start);
    const endPos = model.getPositionAt(detail.end);
    const range = new monaco.Range(
      startPos.lineNumber,
      startPos.column,
      endPos.lineNumber,
      endPos.column,
    );
    editor.revealRangeInCenterIfOutsideViewport(range);
    editor.setSelection(range);
    editor.focus();
  });

  // Reset-to-sample button: drop the persisted buffer and reload the
  // bundled sample. Confirms first so accidental clicks don't nuke
  // a session's worth of edits.
  const resetBtn = document.getElementById("reset-source");
  resetBtn?.addEventListener("click", () => {
    if (!confirm("Discard the current buffer and reload the bundled sample?")) {
      return;
    }
    try {
      localStorage.removeItem(STORAGE_KEY);
    } catch {
      // ignore
    }
    editor.setValue(SAMPLE_SOURCE);
    editor.focus();
  });

  // Splash dismissal — wait for the wasm worker to ack a first call
  // (this both warms the wasm cache and proves it loaded). The
  // editor mounted synchronously above, so by the time the analyzer
  // resolves we're ready to show the layout. On error we still
  // remove the splash; the in-panel error UI takes over.
  void getAnalyzer()
    .diagnostics(initial)
    .catch(() => undefined)
    .finally(() => {
      const splash = document.getElementById("splash");
      if (!splash) return;
      splash.classList.add("splash-hide");
      // Match the CSS opacity transition (200ms) before removing
      // from the DOM so the fade actually plays.
      setTimeout(() => splash.remove(), 250);
    });
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
