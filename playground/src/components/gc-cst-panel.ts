// CST inspection panel. Renders the wasm `parse_tree` output as a
// nested expandable tree of `<wa-details>`. Anonymous tokens get a
// dimmer style; ERROR / MISSING nodes are highlighted.

import { html, css, type TemplateResult } from "lit";
import { customElement } from "lit/decorators.js";
import { GcBasePanel } from "./gc-base-panel.ts";

interface CstNode {
  kind: string;
  field?: string;
  is_named: boolean;
  is_error: boolean;
  is_missing: boolean;
  range: { start: number; end: number };
  text?: string;
  children: CstNode[];
}

@customElement("gc-cst-panel")
export class GcCstPanel extends GcBasePanel {
  static styles = [
    GcBasePanel.styles,
    css`
      .row {
        padding: 1px 0;
        cursor: pointer;
      }
      .row:hover,
      summary:hover {
        background: var(--wa-color-surface-default);
      }
      .anon {
        opacity: 0.55;
      }
      .field {
        color: var(--wa-color-brand-on-normal);
        font-weight: 600;
      }
      .kind {
        color: var(--wa-color-text-normal);
      }
      .text {
        opacity: 0.7;
      }
      .range {
        opacity: 0.45;
        margin-left: 0.5rem;
        font-size: 0.9em;
      }
      .error,
      .missing {
        background: var(--wa-color-danger-fill-loud);
        color: white;
        padding: 0 4px;
        border-radius: 3px;
      }
      details {
        padding-left: 1rem;
      }
      summary {
        cursor: pointer;
        list-style: revert;
      }
    `,
  ];

  protected compute(wasm: any, source: string): TemplateResult {
    const root = wasm.parse_tree(source) as CstNode;
    return html`<div>${this.renderNode(root)}</div>`;
  }

  private renderNode(node: CstNode): TemplateResult {
    const fieldLabel = node.field ? html`<span class="field">${node.field}:</span> ` : null;
    const tag = node.is_error
      ? html`<span class="error">ERROR</span>`
      : node.is_missing
        ? html`<span class="missing">MISSING ${node.kind}</span>`
        : html`<span class="kind ${node.is_named ? "" : "anon"}">(${node.kind})</span>`;
    const range = html`<span class="range">[${node.range.start}–${node.range.end}]</span>`;
    const jump = (e: Event) => {
      e.stopPropagation();
      this.jump(node.range.start, node.range.end);
    };

    if (node.children.length === 0) {
      const text =
        node.text && node.text.length < 40
          ? html` <span class="text">${JSON.stringify(node.text)}</span>`
          : null;
      return html`<div class="row" @click=${jump}>${fieldLabel}${tag}${text}${range}</div>`;
    }

    return html`
      <details open>
        <summary @click=${jump}>${fieldLabel}${tag}${range}</summary>
        ${node.children.map((c) => this.renderNode(c))}
      </details>
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
    "gc-cst-panel": GcCstPanel;
  }
}
