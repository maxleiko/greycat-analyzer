// Flat token list — kind, line:col span, and verbatim text per token.

import { html, css, type TemplateResult } from "lit";
import { customElement } from "lit/decorators.js";
import { GcBasePanel } from "./gc-base-panel.ts";
import type { Analyzer } from "../analyzer-client.ts";

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
        border-bottom: 1px solid var(--wa-color-surface-border);
      }
      th {
        font-weight: 600;
        opacity: 0.8;
      }
      .kind {
        color: var(--wa-color-brand-on-normal);
      }
      .range {
        opacity: 0.6;
        font-variant-numeric: tabular-nums;
      }
      tbody tr {
        cursor: pointer;
      }
      tbody tr:hover {
        background: var(--wa-color-surface-default);
      }
    `,
  ];

  protected async compute(analyzer: Analyzer, source: string): Promise<TemplateResult> {
    const tokens = (await analyzer.tokens(source)) as Token[];
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
              <tr @click=${() => this.jump(t.range.start, t.range.end)}>
                <td class="kind">${t.kind}</td>
                <td class="range">
                  ${t.start.line + 1}:${t.start.column + 1}–${t.end.line + 1}:${t.end.column + 1}
                </td>
                <td>${JSON.stringify(t.text)}</td>
              </tr>
            `,
          )}
        </tbody>
      </table>
    `;
  }

  private jump(start: number, end: number) {
    this.dispatchEvent(
      new CustomEvent("gc-jump", {
        detail: { start, end },
        bubbles: true,
        composed: true,
      }),
    );
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "gc-tokens-panel": GcTokensPanel;
  }
}
