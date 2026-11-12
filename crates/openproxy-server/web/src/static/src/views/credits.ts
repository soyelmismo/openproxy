// views/credits.ts — prepaid credits: balance, top-up, history, quota.
//
// The section covers the logged-in credit flow end to end:
//  - balance with its expiring lots (FIFO: the soonest expiry spends first),
//  - a top-up panel that creates an order and follows it to the chain,
//  - the transaction history, every row pointing into the Base explorer,
//  - the key's dedicated quota section (percentage + time-to-reset, no hard dates).
//
// Checkout logic mirrors web/src/static/checkout.html: the order status is
// polled on the public `/v1/credits/order/{code}` endpoint, so the poll keeps
// working even if the admin session expires mid-checkout.

import { html, type TemplateResult } from "lit-html";
import { createView } from "../lib/view-utils.js";
import { api } from "../state/api.js";
import { showToast } from "../components/toast.js";
import { isFullAdmin } from "../state/auth.js";

interface LotView {
  remaining_usd: string;
  expires_at: string | null;
}

interface Balance {
  key_id: number;
  credit_based: boolean;
  balance_usd: string;
  balance_units: number;
  lots: LotView[];
}

interface TxRow {
  kind: string;
  amount_usd: string;
  remaining_usd: string;
  status: string;
  order_code: string | null;
  tx_hash: string | null;
  basescan_url: string | null;
  created_at: string;
}

interface QuotaWindow {
  cap_usd: number | null;
  spent_usd: number;
  resets_at: string | null;
}

interface QuotaStatus {
  enabled: boolean;
  month: QuotaWindow;
  week: QuotaWindow;
  five_hours: QuotaWindow;
}

interface Packages {
  enabled: boolean;
  ttl_days: number[];
  min_topup_usd: number;
}

interface AdminPackage {
  id: number;
  name: string;
  price_usd: number;
  ttl_days: number;
  config: Record<string, unknown> | null;
  active: boolean;
}

interface AdminOrder {
  id: number;
  order_code: string;
  key_id: number | null;
  amount_units: number;
  base_units: number;
  ttl_days: number;
  status: string;
  tx_hash: string | null;
  package_id: number | null;
  created_at: string;
  paid_at: string | null;
}

interface CreditsData {
  balance: Balance;
  transactions: TxRow[];
  quota: QuotaStatus;
  packages: Packages;
  adminPackages: AdminPackage[];
  orders: AdminOrder[];
}

interface Order {
  order_code: string;
  amount_units: number;
  amount_display: string;
  collector_address: string;
  qr_svg: string;
  expires_at: string;
}

let topupOpen = false;
let activeOrder: Order | null = null;
let pollTimer: ReturnType<typeof setInterval> | null = null;
let countdownTimer: ReturnType<typeof setTimeout> | null = null;

async function loadPackages(): Promise<Packages> {
  const res = await fetch("/v1/credits/packages", { headers: { Accept: "application/json" } });
  const body = (await res.json()) as Packages;
  return body;
}

async function loadData(): Promise<CreditsData> {
  // The admin-only ledger queries run solely for operator sessions: a client
  // (analytics/read) key would otherwise hit two 403s on every view mount.
  const adminExtras: Promise<[AdminPackage[], AdminOrder[]]> = isFullAdmin()
    ? Promise.all([
        api("/credits/packages").catch(() => [] as AdminPackage[]),
        api("/credits/orders?limit=200").catch(() => [] as AdminOrder[]),
      ]) as Promise<[AdminPackage[], AdminOrder[]]>
    : Promise.resolve([[], []]);
  const [balance, transactions, quota, packages, [adminPackages, orders]] = await Promise.all([
    api("/credits/balance"),
    api("/credits/transactions?limit=100"),
    api("/credits/quota"),
    loadPackages(),
    adminExtras,
  ]);
  return {
    balance, transactions, quota, packages,
    adminPackages: adminPackages as AdminPackage[],
    orders: orders as AdminOrder[],
  } as CreditsData;
}

function stopTimers(): void {
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  if (countdownTimer) { clearTimeout(countdownTimer); countdownTimer = null; }
}

/** mm:ss left until the order window closes. */
function renderCountdown(expiresAt: string): void {
  const el = document.getElementById("credits-countdown");
  if (!el) return;
  const deadline = new Date(expiresAt).getTime();
  const left = Math.max(0, Math.floor((deadline - Date.now()) / 1000));
  const m = Math.floor(left / 60);
  const s = left % 60;
  el.textContent = left > 0
    ? `Price holds for ${m}:${String(s).padStart(2, "0")}`
    : "Expired — create a new order";
}

function startCountdown(expiresAt: string): void {
  stopTimers();
  renderCountdown(expiresAt);
  countdownTimer = setInterval(() => renderCountdown(expiresAt), 1000) as unknown as ReturnType<typeof setTimeout>;
}

function startPolling(order: Order, onPaid: () => void): void {
  if (pollTimer) clearInterval(pollTimer);
  pollTimer = setInterval(() => {
    void fetch(`/v1/credits/order/${order.order_code}`, { headers: { Accept: "application/json" } })
      .then((r) => r.json())
      .then((st) => {
        if (st.status === "paid" || st.status === "minted") {
          stopTimers();
          activeOrder = null;
          showToast("Payment confirmed on-chain", "success");
          onPaid();
        }
      })
      .catch(() => { /* transient: keep polling */ });
  }, 5000) as unknown as ReturnType<typeof setInterval>;
}

async function createTopup(): Promise<void> {
  const usdEl = document.getElementById("credits-topup-usd") as HTMLSelectElement | null;
  const ttlEl = document.getElementById("credits-topup-ttl") as HTMLSelectElement | null;
  if (!usdEl || !ttlEl) return;
  try {
    const order = (await api("/credits/topup", {
      method: "POST",
      body: JSON.stringify({
        usd: parseFloat(usdEl.value),
        ttl_days: parseInt(ttlEl.value, 10),
      }),
    })) as unknown as Order;
    activeOrder = order;
    startCountdown(order.expires_at);
    startPolling(order, () => window.location.reload());
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "top-up failed", "error");
  }
}

/** Re-render in place: re-mounts into the surface the view currently lives in
 *  (tile or shell #main), never a detached default container. */
function requestRefresh(): void {
  const el = mountedContainer && mountedContainer.isConnected ? mountedContainer : undefined;
  void mountCredits(el);
}

function pct(win: QuotaWindow): number | null {
  if (win.cap_usd == null || win.cap_usd <= 0) return null;
  return Math.min(100, Math.round((win.spent_usd / win.cap_usd) * 100));
}

/** "in 2d 3h 14m" style time-to-reset; never a hard date (per product rule). */
function timeToReset(iso: string | null): string {
  if (!iso) return "";
  const left = new Date(iso).getTime() - Date.now();
  if (left <= 0) return "now";
  const d = Math.floor(left / 86_400_000);
  const h = Math.floor((left % 86_400_000) / 3_600_000);
  const m = Math.floor((left % 3_600_000) / 60_000);
  if (d > 0) return `resets in ${d}d ${h}h`;
  if (h > 0) return `resets in ${h}h ${m}m`;
  return `resets in ${m}m`;
}

function quotaBar(label: string, win: QuotaWindow): TemplateResult {
  const p = pct(win);
  const cap = win.cap_usd == null ? "no cap" : `$${win.cap_usd.toFixed(2)} cap`;
  return html`
    <div style="margin-bottom:10px">
      <div style="display:flex;justify-content:space-between;font-size:12px;color:var(--muted)">
        <span>${label} — ${cap}</span>
        <span>${p == null ? "" : `${p}% used`}</span>
      </div>
      <div style="height:6px;border-radius:3px;background:var(--border,#232a3d);overflow:hidden">
        <div style="height:100%;width:${p == null ? 0 : p}%;background:var(--accent,#6366f1)"></div>
      </div>
      <div style="font-size:11px;color:var(--muted);margin-top:2px">
        $${win.spent_usd.toFixed(4)} spent ${timeToReset(win.resets_at)}
      </div>
    </div>`;
}

function render(data: CreditsData): TemplateResult {
  const b = data.balance;
  const disabled = !data.packages.enabled;
  return html`
    <div class="page" style="max-width:760px">
      <h2 style="margin:0 0 4px">Credits</h2>
      <p style="color:var(--muted);font-size:13px;margin:0 0 20px">
        Prepaid balance funded with USDC on Base. Lots spend FIFO — soonest expiry first. No refunds.
      </p>

      <div class="card" style="margin-bottom:16px">
        <div style="display:flex;justify-content:space-between;align-items:baseline">
          <div>
            <div style="font-size:13px;color:var(--muted)">Balance</div>
            <div style="font-size:30px;font-weight:700">USDC ${b.balance_usd}</div>
          </div>
          <button class="primary" id="credits-open-topup" ?disabled=${disabled}
                  @click=${() => { topupOpen = !topupOpen; activeOrder = null; requestRefresh(); }}>
            Top up
          </button>
        </div>
        ${b.lots.length === 0
          ? html`<div style="color:var(--muted);font-size:12px;margin-top:8px">No active lots.</div>`
          : html`<div style="margin-top:10px;font-size:12px;color:var(--muted)">
              ${b.lots.map((l) => html`<div>USDC ${l.remaining_usd} — expires ${l.expires_at ?? "never"}</div>`)}
            </div>`}
      </div>

      ${topupOpen && !disabled
        ? html`
      <div class="card" style="margin-bottom:16px">
        <div class="card-title">Top up with USDC on Base</div>
        <div class="card-body">
          ${activeOrder == null
            ? html`
              <label style="display:block;font-size:12px;color:var(--muted);margin:6px 0 4px">Amount (USD)</label>
              <select id="credits-topup-usd" style="width:100%;max-width:200px">
                ${[5, 10, 25, 50, 100].map((v) => html`<option value=${v}>$${v}</option>`)}
              </select>
              <label style="display:block;font-size:12px;color:var(--muted);margin:10px 0 4px">Balance lifetime</label>
              <select id="credits-topup-ttl" style="width:100%;max-width:200px">
                ${data.packages.ttl_days.map((d) => html`<option value=${d} ?selected=${d === 15}>${d} days</option>`)}
              </select>
              <div style="margin-top:14px;display:flex;gap:8px">
                <button class="primary" @click=${() => void createTopup()}>Create payment</button>
                <button class="secondary" @click=${() => { topupOpen = false; requestRefresh(); }}>Cancel</button>
              </div>
              <div style="font-size:12px;color:var(--muted);margin-top:12px">
                No refunds: the balance expires on the lifetime you pick. Unpaid balances are not returned.
              </div>`
            : html`
              <div style="display:flex;gap:18px;align-items:flex-start;flex-wrap:wrap">
                <div style="width:220px" .innerHTML=${activeOrder.qr_svg}></div>
                <div style="flex:1;min-width:200px">
                  <div style="font-size:22px;font-weight:700">USDC ${activeOrder.amount_display}</div>
                  <div style="font-size:11px;color:var(--muted);word-break:break-all;margin:6px 0">
                    to ${activeOrder.collector_address}
                  </div>
                  <div id="credits-countdown" style="font-size:14px;color:var(--warn,#f59e0b)"></div>
                  <div style="font-size:12px;color:var(--muted);margin-top:8px">
                    Send the exact amount from any wallet. The page detects the payment automatically. No refunds.
                  </div>
                </div>
              </div>`}
        </div>
      </div>`
        : null}

      <section class="card" style="margin-bottom:16px">
        <div class="card-title">Quota</div>
        <div class="card-body">
          ${data.quota.enabled
            ? html`
                ${quotaBar("Monthly", data.quota.month)}
                ${quotaBar("Weekly", data.quota.week)}
                ${quotaBar("5 hours", data.quota.five_hours)}`
            : html`<div style="color:var(--muted);font-size:13px">No quota caps on this key.</div>`}
        </div>
      </section>

      <section class="card">
        <div class="card-title">Transaction history</div>
        <div class="card-body" style="font-size:13px">
          ${data.transactions.length === 0
            ? html`<div style="color:var(--muted)">No transactions yet.</div>`
            : html`
          <table style="width:100%;border-collapse:collapse">
            <thead>
              <tr style="color:var(--muted);font-size:11px;text-align:left">
                <th style="padding:4px 8px 4px 0">Date</th>
                <th style="padding:4px 8px">Kind</th>
                <th style="padding:4px 8px">Amount</th>
                <th style="padding:4px 8px">Status</th>
                <th style="padding:4px 0 4px 8px">Chain</th>
              </tr>
            </thead>
            <tbody>
              ${data.transactions.map((t) => html`
              <tr style="border-top:1px solid var(--border,#232a3d)">
                <td style="padding:6px 8px 6px 0;white-space:nowrap">${t.created_at}</td>
                <td style="padding:6px 8px">${t.kind}</td>
                <td style="padding:6px 8px">${t.kind === "topup" ? "+" : ""}${t.amount_usd}</td>
                <td style="padding:6px 8px">${t.status}</td>
                <td style="padding:6px 0 6px 8px">
                  ${t.basescan_url == null
                    ? html`<span style="color:var(--muted)">—</span>`
                    : html`<a href=${t.basescan_url} target="_blank" rel="noopener noreferrer">view on BaseScan</a>`}
                </td>
              </tr>`)}
            </tbody>
          </table>`}
        </div>
      </section>
      ${isFullAdmin() ? renderAdmin(data) : html``}
    </div>
  `;
}


async function createPackage(): Promise<void> {
  const name = (document.getElementById("pkg-name") as HTMLInputElement | null)?.value.trim();
  const price = parseFloat((document.getElementById("pkg-price") as HTMLInputElement | null)?.value ?? "");
  const ttl = parseInt((document.getElementById("pkg-ttl") as HTMLInputElement | null)?.value ?? "", 10);
  const monthly = parseFloat((document.getElementById("pkg-monthly") as HTMLInputElement | null)?.value ?? "");
  if (!name || !Number.isFinite(price) || !Number.isFinite(ttl)) {
    showToast("name, price and ttl_days are required", "error");
    return;
  }
  try {
    await api("/credits/packages", {
      method: "POST",
      body: JSON.stringify({
        name, price_usd: price, ttl_days: ttl,
        config: Number.isFinite(monthly) ? { monthly_usd_cap: monthly } : null,
      }),
    });
    showToast("package created", "success");
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "create failed", "error");
  }
}

async function togglePackage(id: number, active: boolean): Promise<void> {
  try {
    await api(`/credits/packages/${id}`, { method: "PATCH", body: JSON.stringify({ active }) });
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "update failed", "error");
  }
}

function renderAdmin(data: CreditsData): TemplateResult {
  return html`
      <section class="card" style="margin-top:16px">
        <div class="card-title">Packages (presets — copy-paste onto the buyer's key)</div>
        <div class="card-body" style="font-size:13px">
          <table style="width:100%;border-collapse:collapse">
            <thead><tr style="color:var(--muted);font-size:11px;text-align:left">
              <th style="padding:4px 8px 4px 0">Name</th><th style="padding:4px 8px">Price</th>
              <th style="padding:4px 8px">TTL</th><th style="padding:4px 8px">Caps / allowlists</th>
              <th style="padding:4px 8px">Active</th><th></th>
            </tr></thead>
            <tbody>
              ${data.adminPackages.map((pk) => html`
              <tr style="border-top:1px solid var(--border,#232a3d)">
                <td style="padding:6px 8px 6px 0">${pk.name}</td>
                <td style="padding:6px 8px">$${pk.price_usd}</td>
                <td style="padding:6px 8px">${pk.ttl_days}d</td>
                <td style="padding:6px 8px;color:var(--muted)">${pk.config == null ? "—" : JSON.stringify(pk.config)}</td>
                <td style="padding:6px 8px">${pk.active ? "yes" : "no"}</td>
                <td style="padding:6px 0 6px 8px">
                  <button class="secondary" style="padding:2px 8px"
                          @click=${() => void togglePackage(pk.id, !pk.active)}>${pk.active ? "disable" : "enable"}</button>
                </td>
              </tr>`)}
            </tbody>
          </table>
          <div style="margin-top:12px;display:flex;gap:6px;flex-wrap:wrap;align-items:flex-end">
            <input id="pkg-name" placeholder="name (e.g. Starter)" style="max-width:170px">
            <input id="pkg-price" placeholder="price USD" style="max-width:100px">
            <input id="pkg-ttl" placeholder="ttl days" style="max-width:90px">
            <input id="pkg-monthly" placeholder="monthly cap USD" style="max-width:130px">
            <button class="primary" @click=${() => void createPackage()}>Add package</button>
          </div>
          <div style="font-size:11px;color:var(--muted);margin-top:8px">
            Add models/providers/combos to the config JSON via the API for a stricter preset.
          </div>
        </div>
      </section>

      <section class="card" style="margin-top:16px">
        <div class="card-title">Orders (every checkout)</div>
        <div class="card-body" style="font-size:12px">
          <table style="width:100%;border-collapse:collapse">
            <thead><tr style="color:var(--muted);font-size:11px;text-align:left">
              <th style="padding:4px 8px 4px 0">Code</th><th style="padding:4px 8px">Key</th>
              <th style="padding:4px 8px">Amount</th><th style="padding:4px 8px">Pkg</th>
              <th style="padding:4px 8px">Status</th><th style="padding:4px 0 4px 8px">Chain</th>
            </tr></thead>
            <tbody>
              ${data.orders.map((o) => html`
              <tr style="border-top:1px solid var(--border,#232a3d)">
                <td style="padding:6px 8px 6px 0">${o.order_code}</td>
                <td style="padding:6px 8px">${o.key_id ?? "new"}</td>
                <td style="padding:6px 8px">${(o.base_units / 1_000_000).toFixed(2)}</td>
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
      </section>
  `;
}

let mountedContainer: HTMLElement | null = null;

export async function mountCredits(
  target?: HTMLElement | null | string,
): Promise<(() => void) | void> {
  // Same dual-surface pattern as mountAnalytics: the shell renders into
  // #main, workspace tiles pass their own target. Rendering into a detached
  // #main while a tile owns the DOM leaves lit parts orphaned (Text node
  // updates throw on the next refresh).
  const container = target instanceof HTMLElement ? target : document.getElementById("main");
  if (!container || !container.isConnected) return;
  mountedContainer = container;
  return createView<CreditsData>({
    container,
    loader: loadData,
    render,
    cleanup: stopTimers,
  });
}
