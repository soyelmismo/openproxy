// views/credits/shared.ts — module state + the checkout poll machinery.
//
// The panel checkout polls the PUBLIC /v1/credits/order/{code} endpoint, so
// the poll keeps working even if the admin session expires mid-checkout.
// `requestRefresh` re-mounts into the surface the view currently lives in
// (tile or shell #main), never a detached container.

import { showToast } from "../../components/toast.js";
import { api } from "../../state/api.js";
import type { CatalogModel, Order, TierDraft } from "./types.js";

// ── module state ────────────────────────────────────────────────────
export let topupOpen = false;
export let activeOrder: Order | null = null;
export let selectedPackageId: number | null = null;
export let tierDraft: TierDraft | null = null;
export let catalogModels: CatalogModel[] | null = null;

let pollTimer: ReturnType<typeof setInterval> | null = null;
let countdownTimer: ReturnType<typeof setTimeout> | null = null;
let mountedContainer: HTMLElement | null = null;
let remount: ((el?: HTMLElement | null) => Promise<unknown>) | null = null;

export function setTopupOpen(v: boolean): void {
  topupOpen = v;
}
export function setActiveOrder(v: Order | null): void {
  activeOrder = v;
}
export function setSelectedPackageId(v: number | null): void {
  selectedPackageId = v;
}
export function setTierDraft(v: TierDraft | null): void {
  tierDraft = v;
}
export function setCatalogModels(v: CatalogModel[] | null): void {
  catalogModels = v;
}

/** The index view binds itself here so every module can re-render in place. */
export function bindRefresh(
  container: HTMLElement,
  mount: (el?: HTMLElement | null) => Promise<unknown>,
): void {
  mountedContainer = container;
  remount = mount;
}

/** Re-render in place on the live surface (tile or shell #main). */
export function requestRefresh(): void {
  const el = mountedContainer && mountedContainer.isConnected ? mountedContainer : undefined;
  void remount?.(el);
}

// ── timers ──────────────────────────────────────────────────────────
export function stopTimers(): void {
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

export function startCountdown(expiresAt: string): void {
  stopTimers();
  renderCountdown(expiresAt);
  countdownTimer = setInterval(() => renderCountdown(expiresAt), 1000) as unknown as ReturnType<typeof setTimeout>;
}

export function startPolling(order: Order, onPaid: () => void): void {
  if (pollTimer) clearInterval(pollTimer);
  pollTimer = setInterval(() => {
    void fetch(`/v1/credits/order/${order.order_code}`, { headers: { Accept: "application/json" } })
      .then((r) => r.json())
      .then((st) => {
        if (st.status === "paid" || st.status === "minted") {
          stopTimers();
          setActiveOrder(null);
          showToast("Payment confirmed on-chain", "success");
          onPaid();
        }
      })
      .catch(() => { /* transient: keep polling */ });
  }, 5000) as unknown as ReturnType<typeof setInterval>;
}

// ── model catalog (tier editor) ─────────────────────────────────────
export async function ensureCatalog(): Promise<void> {
  if (catalogModels != null) return;
  try {
    const rows = (await api("/models")) as Array<{ model_id: string; provider_id: string; active?: boolean }>;
    setCatalogModels(
      rows
        .filter((m) => m.active !== false)
        .map((m) => ({ id: m.model_id, provider: m.provider_id }))
        .sort((a, b) => a.id.localeCompare(b.id)),
    );
  } catch {
    setCatalogModels([]);
  }
}
