// Monaco-backed editor pane. Emits a `gc-source-change` CustomEvent on
// every keystroke so the parent <gc-playground> can fan out to the
// inspection panels.

import { LitElement, css, html } from "lit";
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
  static styles = css`
    :host {
      display: block;
      width: 100%;
      height: 100%;
    }
    .host {
      width: 100%;
      height: 100%;
    }
  `;

  @property({ type: String }) source = "";

  private editor?: monaco.editor.IStandaloneCodeEditor;
  private hostEl?: HTMLDivElement;
  private suppressChange = false;

  // Render an inert div; Monaco mounts into it after firstUpdated.
  protected render() {
    return html`<div class="host" part="host"></div>`;
  }

  protected firstUpdated() {
    this.hostEl = this.renderRoot.querySelector(".host") as HTMLDivElement;
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
