// views/credits/orders-admin.ts — the full order ledger (admin only):
// every checkout with its charged vs credited split and chain pointer.

import { html, type TemplateResult } from "lit-html";
import type { CreditsData } from "./types.js";

export function renderOrders(data: CreditsData): TemplateResult {
  return html`
    <section class="card" style="margin-top:16px">
      <div class="card-title">Orders (every checkout)</div>
      <div class="card-body" style="font-size:12px">
        <table style="width:100%;border-collapse:collapse">
          <thead><tr style="color:var(--muted);font-size:11px;text-align:left">
            <th style="padding:4px 8px 4px 0">Code</th><th style="padding:4px 8px">Key</th>
            <th style="padding:4px 8px">Charged</th><th style="padding:4px 8px">Credited</th>
            <th style="padding:4px 8px">Pkg</th>
            <th style="padding:4px 8px">Status</th><th style="padding:4px 0 4px 8px">Chain</th>
          </tr></thead>
          <tbody>
            ${data.orders.map((o) => html`
            <tr style="border-top:1px solid var(--border,#232a3d)">
              <td style="padding:6px 8px 6px 0">${o.order_code}</td>
              <td style="padding:6px 8px">${o.key_id ?? "new"}</td>
              <td style="padding:6px 8px">${(o.base_units / 1_000_000).toFixed(2)}</td>
              <td style="padding:6px 8px">${(o.credit_units / 1_000_000).toFixed(2)}</td>
              <td style="padding:6px 8px">${o.package_id ?? "—"}</td>
              <td style="padding:6px 8px">${o.status}</td>
              <td style="padding:6px 0 6px 8px">
                ${o.tx_hash == null
                  ? html`<span style="color:var(--muted)">—</span>`
                  : html`<a href=${`https://basescan.org/tx/${o.tx_hash}`} target="_blank" rel="noopener noreferrer">tx</a>`}
              </td>
            </tr>`)}
          </tbody>
        </table>
      </div>
    </section>`;
}
