// Side-by-side: original source vs. formatted output. Shows whether
// fmt would touch the file ("idempotent" badge when no diff).

import { html, css, type TemplateResult } from "lit";
import { customElement } from "lit/decorators.js";
import { GcBasePanel } from "./gc-base-panel.ts";

@customElement("gc-format-panel")
export class GcFormatPanel extends GcBasePanel {
  static styles = [
    GcBasePanel.styles,
    css`
      .badges {
        margin-bottom: 0.75rem;
        display: flex;
        gap: 0.5rem;
        align-items: center;
      }
      .badge {
        display: inline-block;
        padding: 0 8px;
        border-radius: 3px;
        font-size: 0.85em;
      }
      .clean {
        background: var(--wa-color-success-fill-loud);
        color: white;
      }
      .drift {
        background: var(--wa-color-warning-fill-loud);
        color: white;
      }
      .columns {
        display: grid;
        grid-template-columns: 1fr 1fr;
        gap: 0.75rem;
      }
      .col h4 {
        font-family: var(--wa-font-family-body, sans-serif);
        font-size: 0.95rem;
        margin: 0 0 0.5rem;
        opacity: 0.8;
      }
      pre {
        max-height: 60vh;
        overflow: auto;
        padding: 0.5rem 0.75rem;
        background: var(--wa-color-surface-lowered);
        border-radius: 4px;
      }
    `,
  ];

  protected compute(wasm: any, source: string): TemplateResult {
    const formatted = wasm.format(source) as string;
    const clean = formatted === source;
    return html`
      <div class="badges">
        <span class="badge ${clean ? "clean" : "drift"}">
          ${clean ? "idempotent" : "would reformat"}
        </span>
        ${clean
          ? null
          : html`<span style="opacity:.65"
              >${this.diffSize(source, formatted)}</span
            >`}
      </div>
      <div class="columns">
        <div class="col">
          <h4>input</h4>
          <pre>${source}</pre>
        </div>
        <div class="col">
          <h4>fmt output</h4>
          <pre>${formatted}</pre>
        </div>
      </div>
    `;
  }

  private diffSize(a: string, b: string): string {
    const da = b.length - a.length;
    return `Δ ${da >= 0 ? "+" : ""}${da} bytes`;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "gc-format-panel": GcFormatPanel;
  }
}
