// views/credits/orders-admin.ts — the full order ledger (admin only):
// every checkout with its charged vs credited split and chain pointer.
//
// Server-side search + pagination: the ledger can grow past any client-side
// slice, so `q` / `limit` / `offset` hit GET /credits/orders.
//
// RENDERING CONTRACT (root-cause rule, applies platform-wide): this section
// is rendered by the PARENT template — no nested litRender into a root div.
// The nested pattern breaks when the outer view instance is torn down (lit
// cleans the tree and the nested part becomes a zombie: handlers dead,
// content cleared). Interactions update module state and requestRefresh();
// the view re-renders through the one lit instance that owns the DOM.

import { html, type TemplateResult } from "lit-html";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import { requestRefresh } from "./shared.js";
import { refocusInput } from "./modal.js";
import type { AdminOrder, OrdersPage } from "./types.js";

const PAGE_SIZE = 25;
let query = "";
let offset = 0;
let page: OrdersPage | null = null;
let loading = false;
let debounce: ReturnType<typeof setTimeout> | null = null;

export async function ensureOrdersLoaded(): Promise<void> {
  if (page == null && !loading) await load();
}

async function load(): Promise<void> {
  loading = true;
  try {
    const qs = new URLSearchParams({ limit: String(PAGE_SIZE), offset: String(offset) });
    if (query.trim()) qs.set("q", query.trim());
    page = (await api(`/credits/orders?${qs}`)) as OrdersPage;
  } catch (err) {
    showToast(err instanceof Error ? err.message : "orders load failed", "error");
    page = page ?? { orders: [], total: 0 };
  } finally {
    loading = false;
    requestRefresh();
    // The refresh re-renders the section (and the input): keep the caret.
    refocusInput("orders-search");
  }
}

function onSearch(v: string): void {
  query = v;
  if (debounce) clearTimeout(debounce);
  debounce = setTimeout(() => {
    offset = 0;
    void load();
  }, 300);
}

function rowsTable(): TemplateResult {
  const orders: AdminOrder[] = page?.orders ?? [];
  const total = page?.total ?? 0;
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const current = Math.floor(offset / PAGE_SIZE) + 1;
  return html`
    ${loading && orders.length === 0
      ? html`<div style="color:var(--color-text-muted);padding:8px">loading…</div>`
      : orders.length === 0
        ? html`<div style="color:var(--color-text-muted);padding:8px">no orders${query ? " match this search" : " yet"}</div>`
        : html`
    <table style="width:100%;border-collapse:collapse">
      <thead><tr style="color:var(--color-text-muted);font-size:11px;text-align:left">
        <th style="padding:4px 8px 4px 0">Code</th><th style="padding:4px 8px">Date</th>
        <th style="padding:4px 8px">Key</th><th style="padding:4px 8px">Payer</th>
        <th style="padding:4px 8px">Charged</th><th style="padding:4px 8px">Credited</th>
        <th style="padding:4px 8px">Pkg</th><th style="padding:4px 8px">Status</th>
        <th style="padding:4px 0 4px 8px">Chain</th>
      </tr></thead>
      <tbody>
        ${orders.map((o) => html`
        <tr style="border-top:1px solid var(--color-border)">
          <td style="padding:6px 8px 6px 0;font-family:monospace">${o.order_code}</td>
          <td style="padding:6px 8px;white-space:nowrap;color:var(--color-text-muted)">${o.created_at.slice(0, 16)}</td>
          <td style="padding:6px 8px">${o.key_id ?? "new"}</td>
          <td style="padding:6px 8px;max-width:140px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${o.payer_email ?? "—"}</td>
          <td style="padding:6px 8px">$${(o.base_units / 1_000_000).toFixed(2)}</td>
          <td style="padding:6px 8px">$${(o.credit_units / 1_000_000).toFixed(2)}</td>
          <td style="padding:6px 8px">${o.package_id ?? "—"}</td>
          <td style="padding:6px 8px">${o.status}</td>
          <td style="padding:6px 0 6px 8px">
            ${o.tx_hash == null
              ? html`<span style="color:var(--color-text-muted)">—</span>`
              : html`<a href=${`https://basescan.org/tx/${o.tx_hash}`} target="_blank" rel="noopener noreferrer">tx</a>`}
          </td>
        </tr>`)}
      </tbody>
    </table>`}
    <div style="display:flex;justify-content:space-between;align-items:center;margin-top:6px;font-size:11px;color:var(--color-text-muted)">
      <span>${total === 0 ? "0 orders" : `${offset + 1}–${Math.min(total, offset + PAGE_SIZE)} of ${total}${query ? " (filtered)" : ""}`}</span>
      <span style="display:flex;gap:6px;align-items:center">
        ${loading ? html`<span>…</span>` : null}
        <button class="secondary" style="padding:1px 8px" ?disabled=${current <= 1 || loading}
                @click=${() => { offset = Math.max(0, offset - PAGE_SIZE); void load(); }}>prev</button>
        <span>${current} / ${pages}</span>
        <button class="secondary" style="padding:1px 8px" ?disabled=${current >= pages || loading}
                @click=${() => { offset = (current) * PAGE_SIZE; void load(); }}>next</button>
      </span>
    </div>`;
}

export function renderOrders(): TemplateResult {
  return html`
    <section class="card" style="margin-top:16px">
      <div class="card-title">Orders (every checkout)</div>
      <div class="card-body" style="font-size:12px">
        <div style="display:flex;gap:6px;margin-bottom:8px">
          <input id="orders-search" .value=${query} placeholder="search code, payer email, status or key id…"
                 style="flex:1;max-width:360px"
                 @input=${(e: Event) => onSearch((e.target as HTMLInputElement).value)}>
          ${query
            ? html`<button class="secondary" style="padding:2px 10px"
                     @click=${() => { query = ""; offset = 0; void load(); }}>clear</button>`
            : null}
        </div>
        <div style="overflow-x:auto">${rowsTable()}</div>
      </div>
    </section>`;
}
