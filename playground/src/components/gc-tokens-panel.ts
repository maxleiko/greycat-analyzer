// Flat token list — kind, line:col span, and verbatim text per token.

import { html, css, type TemplateResult } from "lit";
import { customElement } from "lit/decorators.js";
import { GcBasePanel } from "./gc-base-panel.ts";

interface Token {
  kind: string;
  range: { start: number; end: number };
  start: { line: number; column: number };
  end: { line: number; column: number };
  text: string;
}

@customElement("gc-tokens-panel")
export class GcTokensPanel extends GcBasePanel {
  static styles = [
    GcBasePanel.styles,
    css`
      table {
        width: 100%;
        border-collapse: collapse;
      }
      th,
      td {
        text-align: left;
        padding: 2px 8px;
        border-bottom: 1px solid var(--wa-color-neutral-100, #f0f0f0);
      }
      th {
        font-weight: 600;
        opacity: 0.8;
      }
      .kind {
        color: var(--wa-color-brand-primary, #4f8cff);
      }
      .range {
        opacity: 0.6;
        font-variant-numeric: tabular-nums;
      }
    `,
  ];

  protected compute(wasm: any, source: string): TemplateResult {
    const tokens = wasm.tokens(source) as Token[];
    return html`
      <table>
        <thead>
          <tr>
            <th>kind</th>
            <th>line:col</th>
            <th>text</th>
          </tr>
        </thead>
        <tbody>
          ${tokens.map(
            (t) => html`
              <tr>
                <td class="kind">${t.kind}</td>
                <td class="range">
                  ${t.start.line + 1}:${t.start.column + 1}–${t.end.line +
                  1}:${t.end.column + 1}
                </td>
                <td>${JSON.stringify(t.text)}</td>
              </tr>
            `,
          )}
        </tbody>
      </table>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "gc-tokens-panel": GcTokensPanel;
  }
}
