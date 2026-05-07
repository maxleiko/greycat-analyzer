// HIR summary — module name + declaration list + per-arena counts.
// (P5.1 currently only exports the summary; expand to full HIR walk
// when the wasm crate grows that export.)

import { html, css, type TemplateResult } from "lit";
import { customElement } from "lit/decorators.js";
import { GcBasePanel } from "./gc-base-panel.ts";

interface HirSummary {
  module_name: string;
  lib: string;
  decls: { kind: string; name: string; range: { start: number; end: number } }[];
  counts: {
    decls: number;
    stmts: number;
    exprs: number;
    type_refs: number;
    idents: number;
  };
}

@customElement("gc-hir-panel")
export class GcHirPanel extends GcBasePanel {
  static styles = [
    GcBasePanel.styles,
    css`
      h3 {
        font-family: var(--wa-font-family-body, sans-serif);
        font-size: 1rem;
        margin: 0 0 0.5rem;
      }
      .meta {
        opacity: 0.75;
        margin-bottom: 1rem;
      }
      ul {
        list-style: none;
        margin: 0;
        padding: 0;
      }
      li {
        padding: 2px 0;
      }
      .kind {
        color: var(--wa-color-brand-on-normal);
        margin-right: 0.5rem;
        font-weight: 600;
      }
      .counts {
        display: grid;
        grid-template-columns: repeat(5, max-content);
        gap: 0 1rem;
        margin-top: 1rem;
        opacity: 0.85;
      }
      .counts dt {
        font-weight: 600;
      }
      .counts dd {
        margin: 0;
        font-variant-numeric: tabular-nums;
      }
    `,
  ];

  protected compute(wasm: any, source: string): TemplateResult {
    const hir = wasm.lower_hir(source) as HirSummary;
    return html`
      <h3>${hir.module_name}</h3>
      <div class="meta">lib = <code>${hir.lib}</code></div>
      <ul>
        ${hir.decls.map(
          (d) => html`
            <li>
              <span class="kind">${d.kind}</span>${d.name}
              <span class="range" style="opacity:.55"
                >[${d.range.start}–${d.range.end}]</span
              >
            </li>
          `,
        )}
      </ul>
      <dl class="counts">
        <dt>decls</dt>
        <dd>${hir.counts.decls}</dd>
        <dt>stmts</dt>
        <dd>${hir.counts.stmts}</dd>
        <dt>exprs</dt>
        <dd>${hir.counts.exprs}</dd>
        <dt>types</dt>
        <dd>${hir.counts.type_refs}</dd>
        <dt>idents</dt>
        <dd>${hir.counts.idents}</dd>
      </dl>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "gc-hir-panel": GcHirPanel;
  }
}
