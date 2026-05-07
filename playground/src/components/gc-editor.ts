// Monaco-backed editor pane.
//
// Monaco does **not** behave inside a Lit shadow root: it injects its
// own stylesheet into `document.head`, queries computed styles on the
// host, and creates internal positioning elements that need to escape
// any encapsulating shadow tree. The classic symptom (and what we hit
// in the first cut of this component) is a tiny gray rectangle plus
// the "real" source rendered as plaintext underneath.
//
// Fix: render in light DOM by overriding `createRenderRoot()`. The
// element's own width / height comes from the parent layout via the
// global rule in `src/style.css`.

import { LitElement, html } from "lit";
import { customElement, property } from "lit/decorators.js";
import * as monaco from "monaco-editor";

import editorWorkerUrl from "monaco-editor/esm/vs/editor/editor.worker?worker&url";

// Tell Monaco where to find its workers. We only need the base editor
// worker for this playground (no TS / CSS / JSON / HTML support).
self.MonacoEnvironment = {
  getWorkerUrl() {
    return editorWorkerUrl;
  },
};

@customElement("gc-editor")
export class GcEditor extends LitElement {
  @property({ type: String }) source = "";

  private editor?: monaco.editor.IStandaloneCodeEditor;
  private hostEl?: HTMLDivElement;
  private suppressChange = false;

  /** Render in light DOM so Monaco can do its thing. */
  protected createRenderRoot(): HTMLElement | DocumentFragment {
    return this;
  }

  protected render() {
    return html`<div class="gc-editor-host"></div>`;
  }

  protected firstUpdated() {
    this.hostEl = this.renderRoot.querySelector(
      ".gc-editor-host",
    ) as HTMLDivElement;
    this.editor = monaco.editor.create(this.hostEl, {
      value: this.source,
      language: "plaintext",
      theme: matchMedia("(prefers-color-scheme: dark)").matches
        ? "vs-dark"
        : "vs",
      automaticLayout: true,
      minimap: { enabled: false },
      fontFamily: "JetBrains Mono, Fira Code, ui-monospace, monospace",
      fontSize: 13,
      tabSize: 4,
      insertSpaces: true,
      scrollBeyondLastLine: false,
    });

    this.editor.onDidChangeModelContent(() => {
      if (this.suppressChange) return;
      const value = this.editor!.getValue();
      this.dispatchEvent(
        new CustomEvent("gc-source-change", {
          detail: value,
          bubbles: true,
          composed: true,
        }),
      );
    });
  }

  // Reflect external `.source` changes back into the editor (e.g. when
  // the format panel writes back, or future "load fixture" actions).
  protected updated(changed: Map<string, unknown>) {
    if (
      changed.has("source") &&
      this.editor &&
      this.editor.getValue() !== this.source
    ) {
      this.suppressChange = true;
      this.editor.setValue(this.source);
      this.suppressChange = false;
    }
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this.editor?.dispose();
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "gc-editor": GcEditor;
  }
  interface Window {
    MonacoEnvironment?: {
      getWorkerUrl?(moduleId: string, label: string): string;
      getWorker?(moduleId: string, label: string): Worker;
    };
  }
}
