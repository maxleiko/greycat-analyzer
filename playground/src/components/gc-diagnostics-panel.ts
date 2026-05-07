// Merged parse + semantic + lint diagnostics. Empty state is a
// friendly green checkmark.

import { html, css, type TemplateResult } from "lit";
import { customElement } from "lit/decorators.js";
import { GcBasePanel } from "./gc-base-panel.ts";

interface Diagnostic {
  severity: "error" | "warning" | "hint";
  source: string;
  code: string | null;
  message: string;
  range: { start: number; end: number };
  start: { line: number; column: number };
  end: { line: number; column: number };
}

@customElement("gc-diagnostics-panel")
export class GcDiagnosticsPanel extends GcBasePanel {
  static styles = [
    GcBasePanel.styles,
    css`
      .ok {
        color: var(--wa-color-success-fill-loud);
        padding: 0.5rem 0.75rem;
        border: 1px solid currentColor;
        border-radius: 4px;
        background: rgba(0, 200, 0, 0.04);
        font-style: normal;
      }

      ul {
        list-style: none;
        margin: 0;
        padding: 0;
      }
      li {
        padding: 4px 0;
        border-bottom: 1px solid var(--wa-color-surface-border);
      }
      .pos {
        opacity: 0.6;
        margin-right: 0.5rem;
        font-variant-numeric: tabular-nums;
      }
      .badge {
        display: inline-block;
        padding: 0 6px;
        border-radius: 3px;
        font-size: 0.85em;
        margin-right: 0.5rem;
      }
      .error {
        background: var(--wa-color-danger-fill-loud);
        color: white;
      }
      .warning {
        background: var(--wa-color-warning-fill-loud);
        color: white;
      }
      .hint {
        background: var(--wa-color-surface-border);
      }
      .source {
        opacity: 0.6;
        font-size: 0.85em;
      }
    `,
  ];

  protected compute(wasm: any, source: string): TemplateResult {
    const diags = (wasm.diagnostics(source) as Diagnostic[]).slice().sort(
      (a, b) =>
        a.start.line - b.start.line || a.start.column - b.start.column,
    );

    if (diags.length === 0) {
      return html`<div class="ok">no diagnostics 🎉</div>`;
    }

    return html`
      <ul>
        ${diags.map(
          (d) => html`
            <li>
              <span class="pos">
                ${d.start.line + 1}:${d.start.column + 1}
              </span>
              <span class="badge ${d.severity}">${d.severity}</span>
              <span>${d.message}</span>
              <span class="source"
                >(${d.source}${d.code ? ` · ${d.code}` : ""})</span
              >
            </li>
          `,
        )}
      </ul>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "gc-diagnostics-panel": GcDiagnosticsPanel;
  }
}
