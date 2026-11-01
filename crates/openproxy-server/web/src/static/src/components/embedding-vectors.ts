// components/embedding-vectors.ts — the vector-card renderer shared by the
// Playground "Formatted" tab and the log-detail Response tab. One item of an
// `data[]` embedding response becomes one card: a header, a numeric preview,
// and a bar per leading dimension.
//
// The classes are the playground's (`playground-vector-card`, `vector-bar`, …)
// and live in `styles/views/playground_panels.css`, which `views.css` already
// imports into the global `app.css`. No extra stylesheet work is needed to show
// the same visualization inside the log modal.

import { html, type TemplateResult } from "lit-html";
import { t } from "../i18n/index.js";

/** One `data[]` entry of an embeddings response. */
export interface EmbeddingEntry {
  embedding?: number[];
  index?: number;
}

/** A single vector card, shared verbatim between playground and log detail. */
export function renderEmbeddingVectorCard(
  emb: EmbeddingEntry,
  idx: number,
): TemplateResult {
  const vector = emb.embedding || [];
  const preview = vector.slice(0, 8);
  return html`
    <div class="playground-vector-card">
      <h4>${t("playground.embedding.dimensions", { index: idx + 1, count: vector.length })}</h4>
      <div class="playground-vector-preview">
        [${preview.map((v) => v.toFixed(6)).join(", ")}${vector.length > 8 ? ", …" : ""}]
      </div>
      <div class="playground-vector-bars">
        ${vector.slice(0, 80).map((v) => {
          const heightPct = Math.min(100, Math.max(5, Math.abs(v) * 500));
          const isPos = v >= 0;
          return html`<div class="vector-bar ${isPos ? "pos" : "neg"}" style="height: ${heightPct}%;" title="${v}"></div>`;
        })}
      </div>
    </div>
  `;
}

/** Every `data[]` entry as a card row, or the "empty" placeholder. `emptyText`
 *  is the caller's own i18n string so each surface keeps its own message. */
export function renderEmbeddingVectorCards(
  data: EmbeddingEntry[] | undefined,
  emptyText: TemplateResult | string,
): TemplateResult {
  if (!data || data.length === 0) {
    return html`<div class="playground-empty-response">${emptyText}</div>`;
  }
  return html`
    <div class="playground-embedding-results">
      ${data.map((emb, idx) => renderEmbeddingVectorCard(emb, idx))}
    </div>
  `;
}
