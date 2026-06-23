// views/key-usage.ts — per-key usage recap. Reuses the
// /keys/:id/usage + /usage/summary?api_key_id=... endpoints.
//
// MIGRATED to lit-html for atomic DOM updates. The view fetches the
// headline metrics on mount, holds them in module-local state, and
// asks `requestUpdate()` to re-render via lit-html's diffing. No
// `innerHTML` is assigned directly.

import { html, type TemplateResult } from 'lit-html';
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";

// Shape of the payload from /keys/:id/usage. The server hands back
// a small headline object (key metadata + a usage summary) and the
// dashboard just dumps it into a table. We keep both sub-objects
// optional so a partially populated response doesn't crash the
// render — the JS used to be `head.key || {}` and
// `head.summary || {}` for the same reason.
interface KeyUsageHead {
  key?: {
    last_used_at?: string | null;
  } | null;
  summary?: {
    unique_requests?: number;
    total_rows?: number;
    errors?: number;
    total_cost_usd?: number;
  } | null;
}

// ---- Module-local state ----
// Captured by the render closure. `loadError` is set when the
// initial fetch fails so the template can swap the loading view
// for an inline banner (matches the previous innerHTML behaviour).
let keyId: number = 0;
let head: KeyUsageHead | null = null;
let loadError: string | null = null;

function renderKeyUsage(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><a href="#/keys" class="back-link">← All keys</a><h2>API key #${keyId}</h2></div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }
  if (!head) {
    return html`
      <div class="page-header"><a href="#/keys" class="back-link">← All keys</a><h2>API key #${keyId}</h2></div>
      <div class="loading">Loading...</div>
    `;
  }
  const k = head.key ?? {};
  const s = head.summary ?? {};
  const unique: number = s.unique_requests ?? 0;
  const total: number = s.total_rows ?? 0;
  const errors: number = s.errors ?? 0;
  const cost: string = (s.total_cost_usd ?? 0).toFixed(4);
  const last: string = k.last_used_at ?? "never";
  return html`
    <div class="page-header"><a href="#/keys" class="back-link">← All keys</a><h2>API key #${keyId} usage</h2></div>
    <section class="detail-section">
      <div class="section-header"><h3>Headline metrics</h3></div>
      <table>
        <tbody>
          <tr><th>Total rows</th><td>${total}</td></tr>
          <tr><th>Unique requests</th><td>${unique}</td></tr>
          <tr><th>Errors (4xx/5xx)</th><td>${errors}</td></tr>
          <tr><th>Total cost (USD)</th><td>$${cost}</td></tr>
          <tr><th>Last used</th><td>${last}</td></tr>
        </tbody>
      </table>
    </section>
    <p class="empty"><small>Filter the global Analytics page with <code>?api_key_id=${keyId}</code> for per-(provider, model) breakdown.</small></p>
  `;
}

export async function mountKeyUsage(id: number): Promise<(() => void) | void> {
  const main = document.getElementById("main");
  if (!main) return;
  keyId = id;
  head = null;
  loadError = null;
  const cleanup = mountView(main, renderKeyUsage);
  try {
    head = (await api(`/keys/${id}/usage`)) as KeyUsageHead;
    requestUpdate();
  } catch (e: unknown) {
    loadError = e instanceof Error ? e.message : String(e);
    requestUpdate();
  }
  return cleanup;
}
