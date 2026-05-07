// Per-expression inferred types from the analyzer's expr_types table.
// Sorted by source position so the list reads top-down.

import { html, css, type TemplateResult } from "lit";
import { customElement } from "lit/decorators.js";
import { GcBasePanel } from "./gc-base-panel.ts";

interface ExprType {
  range: { start: number; end: number };
  ty: string;
}

@customElement("gc-types-panel")
export class GcTypesPanel extends GcBasePanel {
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
      .range {
        opacity: 0.55;
        font-variant-numeric: tabular-nums;
      }
      .ty {
        color: var(--wa-color-brand-primary, #4f8cff);
        font-weight: 600;
      }
      .src {
        opacity: 0.7;
      }
    `,
  ];

  protected compute(wasm: any, source: string): TemplateResult {
    const list = (wasm.infer_types(source) as ExprType[]).slice().sort(
      (a, b) => a.range.start - b.range.start,
    );
    return html`
      <table>
        <thead>
          <tr>
            <th>byte range</th>
            <th>type</th>
            <th>source</th>
          </tr>
        </thead>
        <tbody>
          ${list.map(
            (t) => html`
              <tr>
                <td class="range">[${t.range.start}–${t.range.end}]</td>
                <td class="ty">${t.ty}</td>
                <td class="src">
                  ${JSON.stringify(
                    source.slice(t.range.start, t.range.end).slice(0, 60),
                  )}
                </td>
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
    "gc-types-panel": GcTypesPanel;
  }
}
