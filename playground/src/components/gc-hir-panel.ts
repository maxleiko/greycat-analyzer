// Full HIR tree — every decl walked recursively into its statements,
// expressions, type-refs, and arena-resident sub-decls. Each node
// shows kind + an optional label (ident text, op tag, primitive
// name) + its source span. Click jumps Monaco to that span.

import { html, css, type TemplateResult } from "lit";
import { customElement } from "lit/decorators.js";
import { GcBasePanel } from "./gc-base-panel.ts";
import type { Analyzer } from "../analyzer-client.ts";

interface HirNode {
  kind: string;
  label: string | null;
  range: { start: number; end: number };
  children: HirNode[];
}

interface HirRoot {
  module_name: string;
  lib: string;
  counts: {
    decls: number;
    stmts: number;
    exprs: number;
    type_refs: number;
    idents: number;
  };
  decls: HirNode[];
}

@customElement("gc-hir-panel")
export class GcHirPanel extends GcBasePanel {
  static styles = [
    GcBasePanel.styles,
    css`
      h3 {
        font-family: var(--wa-font-family-body, sans-serif);
        font-size: 1rem;
        margin: 0 0 0.25rem;
      }
      .meta {
        opacity: 0.75;
        margin-bottom: 0.75rem;
      }
      .counts {
        display: grid;
        grid-template-columns: repeat(5, max-content);
        gap: 0 1rem;
        margin: 0 0 1rem;
        opacity: 0.85;
        font-size: 0.85em;
      }
      .counts dt {
        font-weight: 600;
      }
      .counts dd {
        margin: 0;
        font-variant-numeric: tabular-nums;
      }
      details {
        padding-left: 1rem;
      }
      summary {
        cursor: pointer;
        list-style: none;
        padding: 1px 0;
      }
      summary::-webkit-details-marker {
        display: none;
      }
      summary:hover,
      .row:hover {
        background: var(--wa-color-surface-default);
      }
      .row {
        padding: 1px 0 1px 1em;
        cursor: pointer;
      }
      .marker {
        display: inline-block;
        width: 1em;
        text-align: center;
        cursor: pointer;
        opacity: 0.6;
        user-select: none;
        transition: transform 0.12s ease-out;
      }
      .marker:hover {
        opacity: 1;
      }
      details[open] > summary > .marker {
        transform: rotate(90deg);
      }
      .kind {
        color: var(--wa-color-brand-on-normal);
        font-weight: 600;
      }
      .kind-prefix {
        opacity: 0.55;
      }
      .label {
        opacity: 0.85;
      }
      .range {
        opacity: 0.45;
        margin-left: 0.5rem;
        font-size: 0.9em;
      }
    `,
  ];

  protected async compute(analyzer: Analyzer, source: string): Promise<TemplateResult> {
    const hir = (await analyzer.lower_hir(source)) as HirRoot;
    return html`
      <h3>${hir.module_name}</h3>
      <div class="meta">lib = <code>${hir.lib}</code></div>
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
      ${hir.decls.map((d) => this.renderNode(d))}
    `;
  }

  /// Render one HIR node. Leafs (no children) emit a `.row`; internal
  /// nodes use `<details>` with a manual marker so only the marker
  /// click toggles the subtree — clicking the rest of the row jumps
  /// Monaco to the node's source span.
  private renderNode(node: HirNode): TemplateResult {
    const kindHtml = this.renderKind(node.kind);
    const label = node.label ? html` <span class="label">${node.label}</span>` : null;
    const range =
      node.range.end > node.range.start
        ? html`<span class="range">[${node.range.start}–${node.range.end}]</span>`
        : null;

    if (node.children.length === 0) {
      const onClick = (e: MouseEvent) => {
        e.stopPropagation();
        this.jump(node.range.start, node.range.end);
      };
      return html`<div class="row" @click=${onClick}>${kindHtml}${label}${range}</div>`;
    }

    const onSummaryClick = (e: MouseEvent) => {
      e.preventDefault();
      this.jump(node.range.start, node.range.end);
    };
    const onMarkerClick = (e: MouseEvent) => {
      e.stopPropagation();
      const target = e.currentTarget as HTMLElement;
      const det = target.closest("details");
      if (det) det.open = !det.open;
    };

    return html`
      <details open>
        <summary @click=${onSummaryClick}>
          <span class="marker" @click=${onMarkerClick}>▸</span>${kindHtml}${label}${range}
        </summary>
        ${node.children.map((c) => this.renderNode(c))}
      </details>
    `;
  }

  /// Split `expr:ident` / `stmt:var` into a dim "expr:" / "stmt:"
  /// prefix + a colored kind chip so the tree's category structure is
  /// scannable at a glance.
  private renderKind(kind: string): TemplateResult {
    const idx = kind.indexOf(":");
    if (idx >= 0) {
      const prefix = kind.slice(0, idx + 1);
      const tail = kind.slice(idx + 1);
      return html`<span class="kind-prefix">${prefix}</span><span class="kind">${tail}</span>`;
    }
    return html`<span class="kind">${kind}</span>`;
  }

  private jump(start: number, end: number) {
    if (end <= start) return;
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
    "gc-hir-panel": GcHirPanel;
  }
}
