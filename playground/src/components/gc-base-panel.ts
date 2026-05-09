// Tiny base class shared by every inspection panel. Owns the worker
// dispatch / inflight ticket / error display dance so each panel just
// implements `compute(source)` returning a Promise<TemplateResult>.
//
// As of the worker refactor (P14.7 stage A) the wasm now lives in a
// dedicated Web Worker; panels make async calls via the typed
// `getAnalyzer()` proxy. The main thread does no parsing / lowering,
// so Monaco stays responsive even on noisy keystrokes.

import { LitElement, html, css, type CSSResultGroup, type TemplateResult } from "lit";
import { property, state } from "lit/decorators.js";
import { getAnalyzer, type Analyzer } from "../analyzer-client.ts";

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
      color: var(--wa-color-danger-fill-loud);
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
      void this.recompute();
    }
  }

  private async recompute() {
    const ticket = ++this.inflight;
    try {
      const next = await this.compute(getAnalyzer(), this.source);
      if (ticket !== this.inflight) return;
      this.error = null;
      this.output = next;
    } catch (e) {
      if (ticket !== this.inflight) return;
      this.error = String((e as Error)?.message ?? e);
    }
  }

  /// Compute the rendered output for `source`. Panels make their
  /// wasm calls through `analyzer` (a typed proxy backed by a Web
  /// Worker) and return a Promise<TemplateResult>.
  protected abstract compute(analyzer: Analyzer, source: string): Promise<TemplateResult>;

  render() {
    if (this.error) {
      return html`<div class="error">${this.error}</div>`;
    }
    return this.output ?? html`<div class="empty">computing…</div>`;
  }
}
