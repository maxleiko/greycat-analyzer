// Root element. Owns the source string, lays out the editor (left)
// next to a tab group of inspection panels (right). Each panel
// receives `source` as a reactive property and re-renders on edit.

import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";
import { SAMPLE_SOURCE } from "../sample.ts";

@customElement("gc-playground")
export class GcPlayground extends LitElement {
  static styles = css`
    :host {
      display: flex;
      height: 100vh;
      width: 100vw;
      font-family: var(--code-font, monospace);
    }

    wa-split-panel {
      flex: 1;
      --divider-width: 4px;
    }

    .editor-pane {
      height: 100%;
      box-sizing: border-box;
      border-right: 1px solid var(--wa-color-neutral-200, #e0e0e0);
    }

    .panels {
      height: 100%;
      box-sizing: border-box;
      display: flex;
      flex-direction: column;
    }

    wa-tab-group {
      flex: 1;
      display: flex;
      flex-direction: column;
      overflow: hidden;
      --indicator-color: var(--wa-color-brand-primary, #4f8cff);
    }

    wa-tab-group::part(body) {
      flex: 1;
      overflow: auto;
    }

    wa-tab-panel {
      height: 100%;
      box-sizing: border-box;
      padding: 0.75rem 1rem;
      overflow: auto;
    }

    header {
      display: flex;
      align-items: center;
      gap: 0.5rem;
      padding: 0.5rem 0.75rem;
      border-bottom: 1px solid var(--wa-color-neutral-200, #e0e0e0);
      font-family: var(--wa-font-family-body, sans-serif);
      font-weight: 600;
    }

    .badge {
      margin-left: auto;
      font-weight: 400;
      font-size: 0.85em;
      opacity: 0.7;
    }
  `;

  @state() private source = SAMPLE_SOURCE;

  private onSourceChange = (e: CustomEvent<string>) => {
    this.source = e.detail;
  };

  render() {
    return html`
      <wa-split-panel position="40">
        <div slot="start" class="editor-pane">
          <gc-editor
            .source=${this.source}
            @gc-source-change=${this.onSourceChange}
          ></gc-editor>
        </div>
        <div slot="end" class="panels">
          <header>
            greycat-analyzer playground
            <span class="badge">${this.source.length} bytes</span>
          </header>
          <wa-tab-group>
            <wa-tab slot="nav" panel="diagnostics">Diagnostics</wa-tab>
            <wa-tab slot="nav" panel="cst">CST</wa-tab>
            <wa-tab slot="nav" panel="tokens">Tokens</wa-tab>
            <wa-tab slot="nav" panel="hir">HIR</wa-tab>
            <wa-tab slot="nav" panel="types">Types</wa-tab>
            <wa-tab slot="nav" panel="format">Format</wa-tab>

            <wa-tab-panel name="diagnostics">
              <gc-diagnostics-panel .source=${this.source}></gc-diagnostics-panel>
            </wa-tab-panel>
            <wa-tab-panel name="cst">
              <gc-cst-panel .source=${this.source}></gc-cst-panel>
            </wa-tab-panel>
            <wa-tab-panel name="tokens">
              <gc-tokens-panel .source=${this.source}></gc-tokens-panel>
            </wa-tab-panel>
            <wa-tab-panel name="hir">
              <gc-hir-panel .source=${this.source}></gc-hir-panel>
            </wa-tab-panel>
            <wa-tab-panel name="types">
              <gc-types-panel .source=${this.source}></gc-types-panel>
            </wa-tab-panel>
            <wa-tab-panel name="format">
              <gc-format-panel .source=${this.source}></gc-format-panel>
            </wa-tab-panel>
          </wa-tab-group>
        </div>
      </wa-split-panel>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "gc-playground": GcPlayground;
  }
}
