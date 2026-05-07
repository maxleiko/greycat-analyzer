// Tiny base class shared by every inspection panel. Owns the wasm
// loading / debounce / error display dance so each panel just
// implements `compute(source)` returning the rendered template.

import {
  LitElement,
  html,
  css,
  type CSSResultGroup,
  type TemplateResult,
} from "lit";
import { property, state } from "lit/decorators.js";
import { getWasm } from "../wasm.ts";

type Wasm = Awaited<ReturnType<typeof getWasm>>;

export abstract class GcBasePanel extends LitElement {
  static styles: CSSResultGroup = css`
    :host {
      display: block;
      font-family: var(--code-font, ui-monospace, monospace);
      font-size: 12px;
      line-height: 1.55;
    }

    pre {
      margin: 0;
      white-space: pre-wrap;
      word-break: break-word;
      font-family: inherit;
      font-size: inherit;
      line-height: inherit;
    }

    .empty {
      opacity: 0.6;
      font-style: italic;
    }

    .error {
      color: var(--wa-color-danger-default, #c00);
      padding: 0.5rem 0.75rem;
      border: 1px solid currentColor;
      border-radius: 4px;
      background: rgba(255, 0, 0, 0.04);
    }
  `;

  @property({ type: String }) source = "";
  @state() protected error: string | null = null;
  @state() protected output: TemplateResult | null = null;

  private inflight = 0;

  protected updated(changed: Map<string, unknown>) {
    if (changed.has("source")) {
      this.recompute();
    }
  }

  private async recompute() {
    const ticket = ++this.inflight;
    try {
      const wasm = await getWasm();
      if (ticket !== this.inflight) return;
      this.error = null;
      this.output = this.compute(wasm, this.source);
    } catch (e) {
      this.error = String((e as Error)?.message ?? e);
    }
  }

  protected abstract compute(wasm: Wasm, source: string): TemplateResult;

  render() {
    if (this.error) {
      return html`<div class="error">${this.error}</div>`;
    }
    return this.output ?? html`<div class="empty">computing…</div>`;
  }
}
