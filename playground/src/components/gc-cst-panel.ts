// CST inspection panel. Renders the wasm `parse_tree` output as a
// nested expandable tree of `<wa-details>`. Anonymous tokens get a
// dimmer style; ERROR / MISSING nodes are highlighted.

import { html, css, type TemplateResult } from "lit";
import { customElement } from "lit/decorators.js";
import { GcBasePanel } from "./gc-base-panel.ts";
import type { Analyzer } from "../analyzer-client.ts";

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
      /* Hide the native disclosure marker — we render our own
         clickable arrow so ONLY the arrow toggles the tree. The
         rest of the summary acts as a "select node" affordance
         (jumps Monaco to the source span). */
      summary {
        cursor: pointer;
        list-style: none;
      }
      summary::-webkit-details-marker {
        display: none;
      }
      .marker {
        display: inline-block;
        width: 1em;
        text-align: center;
        cursor: pointer;
        opacity: 0.6;
        user-select: none;
      }
      .marker:hover {
        opacity: 1;
      }
      details[open] > summary > .marker {
        transform: rotate(90deg);
      }
      .marker {
        transition: transform 0.12s ease-out;
      }
    `,
  ];

  protected async compute(analyzer: Analyzer, source: string): Promise<TemplateResult> {
    const root = (await analyzer.parse_tree(source)) as CstNode;
    return html`<div>${this.renderNode(root)}</div>`;
  }

  private renderNode(node: CstNode): TemplateResult {
    const fieldLabel = node.field ? html`<span class="field">${node.field}:</span> ` : null;
    // Anonymous tokens (`(type)` whose text is also `"type"`,
    // punctuation like `(()` whose text is `"("`) duplicate
    // information. Drop the parenthesized kind chip when it would
    // restate the literal text, but keep field labels so
    // `field: "("` stays unambiguous.
    const showKindChip = !(!node.is_named && node.text && node.text === node.kind);
    const tag = node.is_error
      ? html`<span class="error">ERROR</span>`
      : node.is_missing
        ? html`<span class="missing">MISSING ${node.kind}</span>`
        : showKindChip
          ? html`<span class="kind ${node.is_named ? "" : "anon"}">(${node.kind})</span>`
          : null;
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

    // The marker is the ONLY part that toggles `<details>`. Clicking
    // anywhere else on the summary jumps to the source span without
    // collapsing/expanding the subtree. `preventDefault()` on the
    // summary's own click suppresses the browser's default toggle;
    // the marker's click handler then runs `details.open = !open`.
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
          <span class="marker" @click=${onMarkerClick}>▸</span>${fieldLabel}${tag}${range}
        </summary>
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
