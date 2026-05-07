// Root element. Owns the source string, lays out the editor (left)
// next to a tab group of inspection panels (right). Each panel
// receives `source` as a reactive property and re-renders on edit.

import { LitElement, css, html } from "lit";
import { customElement, state } from "lit/decorators.js";
import { SAMPLE_SOURCE } from "../sample.ts";

@customElement("gc-playground")
export class GcPlayground extends LitElement {
  static styles = css`
    :host {
      display: grid;
      grid-template-columns: 1fr;
      grid-template-rows: 100vh;
      height: 100vh;
      width: 100vw;
      box-sizing: border-box;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font-family: var(--wa-font-family-body, system-ui, sans-serif);
    }

    wa-split-panel {
      width: 100%;
      height: 100%;
      --divider-width: 4px;
    }

    .editor-pane {
      width: 100%;
      height: 100%;
      box-sizing: border-box;
      overflow: hidden;
      background: var(--wa-color-surface-lowered);
    }

    .panels {
      display: flex;
      flex-direction: column;
      width: 100%;
      height: 100%;
      box-sizing: border-box;
      overflow: hidden;
      background: var(--wa-color-surface-default);
    }

    header.bar {
      display: flex;
      align-items: center;
      gap: 0.5rem;
      padding: 0.5rem 1rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
      font-weight: 600;
      font-size: 0.95rem;
    }

    header.bar .badge {
      margin-left: auto;
      font-weight: 400;
      font-size: 0.85em;
      color: var(--wa-color-text-quiet);
    }

    wa-tab-group {
      flex: 1 1 auto;
      min-height: 0;
      display: flex;
      flex-direction: column;
    }

    wa-tab-group::part(body) {
      flex: 1 1 auto;
      overflow: auto;
    }

    wa-tab-panel {
      display: block;
      padding: 0.75rem 1rem;
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
          <header class="bar">
            <span>greycat-analyzer playground</span>
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
