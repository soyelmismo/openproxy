// views/key-usage.js — per-key usage recap. Reuses the
// /keys/:id/usage + /usage/summary?api_key_id=... endpoints.

import { api } from "../state/api.js";
import { escapeHtml } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";

export async function mountKeyUsage(keyId) {
  const main = document.getElementById("main");
  main.innerHTML = pageHeader({ title: `API key #${keyId}` }) + `<div class="loading">Loading...</div>`;
  try {
    const head = await api(`/keys/${keyId}/usage`);
    const k = head.key || {};
    const s = head.summary || {};
    const unique = s.unique_requests ?? 0;
    const total = s.total_rows ?? 0;
    const errors = s.errors ?? 0;
    const cost = (s.total_cost_usd ?? 0).toFixed(4);
    const last = k.last_used_at || "never";
    main.innerHTML = `
      ${pageHeader({ title: `API key #${keyId} usage`, back: { href: "#/keys", label: "← All keys" } })}
      <section class="detail-section">
        <div class="section-header"><h3>Headline metrics</h3></div>
        <table>
          <tbody>
            <tr><th>Total rows</th><td>${total}</td></tr>
            <tr><th>Unique requests</th><td>${unique}</td></tr>
            <tr><th>Errors (4xx/5xx)</th><td>${errors}</td></tr>
            <tr><th>Total cost (USD)</th><td>$${cost}</td></tr>
            <tr><th>Last used</th><td>${escapeHtml(last)}</td></tr>
          </tbody>
        </table>
      </section>
      <p class="empty"><small>Filter the global Analytics page with <code>?api_key_id=${keyId}</code> for per-(provider, model) breakdown.</small></p>
    `;
  } catch (e) {
    main.innerHTML = pageHeader({ title: `API key #${keyId}` }) +
      `<div class="banner banner-error">${escapeHtml(e.message)}</div>`;
  }
}
