// views/providers.ts — provider grid + provider detail (lit-html).
//
// MIGRATED to lit-html. The previous implementation split this view
// across three files (providers.ts, provider-grid.ts,
// provider-detail.ts) and rebuilt the DOM via `innerHTML`. The
// lit-html diff updates only the DOM nodes that actually changed:
//
//   - The models table re-paints only the rows whose `active` flag
//     flipped when the user clicks "Enable all" / "Disable all"
//     (the lag the operator reported — `requestUpdate()` instead
//     of a full innerHTML rebuild).
//   - The search input keeps focus while the user types.
//   - Filter tab clicks update only the tbody + the tab indicator.
//   - Sort header clicks update only the indicator + the row order.
//
// The view entry point is `mountProviders({ detailId })`. When
// `detailId` is provided, the detail view is rendered; otherwise
// the grid is rendered.

import { html, type TemplateResult } from 'lit-html';
// unsafeHTML import removed — all components now return TemplateResult.
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { showCreateProvider } from "../handlers/provider-handlers.js";
import { showCreateAccount, showUpdateAccountKey } from "../handlers/account-handlers.js";
import { showCustomModelForm } from "../components/model-custom-form.js";
import { OAuthLogin } from "../handlers/oauth-handlers.js";
import { renderQuotaCell } from "./quota-cell.js";
import { statusPillClass } from "../lib/constants.js";
import {
  applySort,
  SORTABLE_COLUMNS,
  type ModelSort,
  type SortableColumn,
} from "../components/model-table.js";
import type { Account, Model, Provider, HealthStatus } from "../lib/types/api.js";

// ---- Constants ----

interface ProviderDetailUiState {
  filter: "all" | "active" | "inactive";
  search: string;
  sort: ModelSort | null;
}

// ---- Module-local state ----

let detailProviderId: string | null = null;
let loadError: string | null = null;

// ---- Helpers ----

function getProviderUi(providerId: string): ProviderDetailUiState {
  const raw = state.providerDetail[providerId] as Partial<ProviderDetailUiState> | undefined;
  return {
    filter: raw?.filter ?? "all",
    search: raw?.search ?? "",
    sort: raw?.sort ?? null,
  };
}

function setProviderUi(providerId: string, ui: ProviderDetailUiState): void {
  state.providerDetail[providerId] = ui as unknown as Record<string, unknown>;
}

// Three built-in providers get distinct visual markers; custom
// providers fall back to the first letter of their id (uppercased).
function providerGlyph(providerId: string): string {
  const knownLogos: Record<string, string> = {
    "openrouter": "🟢",
    "minimax": "🟡",
    "opencode-zen": "🟣",
  };
  return knownLogos[providerId] || ((providerId[0] || "?").toUpperCase());
}

function formatContext(tokens: number | null | undefined): TemplateResult {
  if (tokens == null) return html`<span class="muted">—</span>`;
  if (tokens >= 1_000_000) return html`${(tokens / 1_000_000).toFixed(1)}M`;
  if (tokens >= 1000) return html`${Math.round(tokens / 1000)}k`;
  return html`${String(tokens)}`;
}

// Render the per-model capability badges. The server serialises
// capabilities as a JSON string; bad input renders as an em-dash
// rather than throwing.
function renderCapabilityBadges(json: string | null | undefined): TemplateResult {
  if (json == null) return html`<span class="muted">—</span>`;
  let caps: unknown;
  if (typeof json === "string") {
    try { caps = JSON.parse(json) as unknown; } catch (_e: unknown) { return html`<span class="muted">—</span>`; }
  } else {
    caps = json;
  }
  if (!caps || typeof caps !== "object") return html`<span class="muted">—</span>`;
  const c: Record<string, unknown> = caps as Record<string, unknown>;
  const badges: TemplateResult[] = [];
  if (c["vision"]) badges.push(html`<span class="cap-badge">vision</span>`);
  if (c["tool_calling"]) badges.push(html`<span class="cap-badge">tools</span>`);
  if (c["reasoning"]) badges.push(html`<span class="cap-badge">reasoning</span>`);
  if (c["thinking"]) badges.push(html`<span class="cap-badge">thinking</span>`);
  if (c["structured_output"]) badges.push(html`<span class="cap-badge">json</span>`);
  if (c["attachment"]) badges.push(html`<span class="cap-badge">attach</span>`);
  return badges.length > 0 ? html`${badges}` : html`<span class="muted">—</span>`;
}

// Briefly paint a button a colour to confirm a click landed.
function flashButton(btn: HTMLButtonElement | null, text: string, color: string): void {
  if (!btn) return;
  btn.textContent = text;
  btn.style.background = color;
  setTimeout(() => { btn.style.background = ""; }, 1500);
}

// ---- Handlers: grid ----

async function onRefreshAllProviders(): Promise<void> {
  try {
    const providers = await api("/providers") as Array<{ id: string }>;
    for (const p of providers) {
      try {
        await api("/providers/" + encodeURIComponent(p.id) + "/refresh", { method: "POST" });
      } catch (err: unknown) {
        console.error("Failed to refresh", p.id, err);
      }
    }
    state.providers = await api("/providers") as typeof state.providers;
    state.models = await api("/models") as typeof state.models;
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

function onShowCreateProvider(): void {
  showCreateProvider();
}

// ---- Handlers: detail header ----

async function onRenameProvider(providerId: string, currentName: string): Promise<void> {
  const newName = prompt(`Rename provider "${providerId}":`, currentName);
  if (newName == null) return;
  const trimmed = newName.trim();
  if (trimmed === "") {
    showToast("Name cannot be empty", "error");
    return;
  }
  if (trimmed === currentName) return;
  const collision = state.providers.find((p) => p.id !== providerId && p.name === trimmed);
  if (collision) {
    if (!confirm(`A provider with this name already exists (${collision.id}). Use this name anyway?`)) return;
  }
  try {
    await api("/providers/" + encodeURIComponent(providerId), {
      method: "PATCH",
      body: JSON.stringify({ name: trimmed }),
    });
    state.providers = await api("/providers") as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

async function onRefreshProvider(providerId: string, e: Event | null): Promise<void> {
  const target = e && e.target && e.target instanceof HTMLButtonElement ? e.target : null;
  const btn: HTMLButtonElement | null = target;
  const original = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "Refreshing...";
  }
  try {
    const result = (await api(
      "/providers/" + encodeURIComponent(providerId) + "/refresh",
      { method: "POST" },
    )) as { models_refreshed?: number; new_model_ids?: string[] } | null;
    const n: number = (result && typeof result.models_refreshed === "number") ? result.models_refreshed : 0;
    const newIds: string[] = (result && Array.isArray(result.new_model_ids)) ? result.new_model_ids : [];
    const summary: string = n === 0
      ? `Nothing to refresh for ${providerId}.`
      : `Refreshed ${n} models for ${providerId}.`;
    const newSuffix: string = newIds.length === 0
      ? ""
      : newIds.length <= 3
        ? ` New: ${newIds.join(", ")}.`
        : ` New: ${newIds.slice(0, 3).join(", ")} (+${newIds.length - 3} more).`;
    showToast(summary + newSuffix, "success");
    state.providers = await api("/providers") as typeof state.providers;
    state.models = await api("/models") as typeof state.models;
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = original;
    }
  }
}

async function onToggleProviderActive(providerId: string, newActive: boolean): Promise<void> {
  if (!newActive) {
    const ok = confirm(
      `Deactivate provider "${providerId}"?\n\n` +
      `Its accounts and models will be preserved, but it won't be ` +
      `usable in combos until you reactivate it.`
    );
    if (!ok) return;
  }
  try {
    await api("/providers/" + encodeURIComponent(providerId) + "/active", {
      method: "POST",
      body: JSON.stringify({ active: newActive }),
    });
    state.providers = await api("/providers") as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

async function onConfirmDeleteProvider(providerId: string): Promise<void> {
  const typed = prompt(`Type the provider ID to confirm deletion: ${providerId}`);
  if (typed !== providerId) {
    if (typed != null) showToast(`Provider id "${typed}" does not match. Nothing was deleted.`, "error");
    return;
  }
  if (!confirm(`Really delete ${providerId}? This cascades to all its accounts and models.`)) return;
  try {
    await api("/providers/" + encodeURIComponent(providerId), { method: "DELETE" });
    state.providers = state.providers.filter((p) => p.id !== providerId);
    state.models = state.models.filter((m) => m.provider_id !== providerId);
    state.accounts = state.accounts.filter((a) => a.provider_id !== providerId);
    location.hash = "#/providers";
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Cannot delete: " + msg, "error");
  }
}

// ---- Handlers: OAuth section ----

function onOAuthStartPKCE(providerId: string): void { void OAuthLogin.startPKCE(providerId); }
function onOAuthStartDeviceCode(providerId: string): void { void OAuthLogin.startDeviceCode(providerId); }
function onOAuthSubmitManualCallback(): void { void OAuthLogin.submitManualCallback(); }

function onCopyAuthUrl(): void {
  const el = document.getElementById("oauth-auth-url") as HTMLInputElement | null;
  if (el && navigator.clipboard) {
    navigator.clipboard.writeText(el.value || "").catch(() => { /* ignore */ });
  }
}

// ---- Handlers: connections (accounts) ----

async function onSetHealth(id: number, e: Event | null): Promise<void> {
  const target = e && e.target && e.target instanceof HTMLSelectElement ? e.target : null;
  const health = target ? (target.value as HealthStatus) : null;
  if (!health) return;
  try {
    await api("/accounts/" + id + "/health", {
      method: "POST",
      body: JSON.stringify({ health }),
    });
    const a = (state.accounts || []).find((x) => x.id === id);
    if (a) a.health_status = health;
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

async function onRefreshAccountQuota(accountId: number, e: Event | null): Promise<void> {
  const target = e && e.target && e.target instanceof HTMLButtonElement ? e.target : null;
  const btn: HTMLButtonElement | null = target;
  const oldText = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "...";
  }
  try {
    const result = (await api("/accounts/" + accountId + "/refresh-quota", { method: "POST" })) as
      { supported?: boolean; error?: string; model_details?: Array<unknown> } | null;
    if (result && result.supported === false) {
      if (btn) flashButton(btn, "n/a", "#9399b2");
    } else if (result && result.error) {
      if (btn) flashButton(btn, "✗ err", "#f38ba8");
    } else {
      if (btn) flashButton(btn, "✓", "#a6e3a1");
    }
    state.accounts = await api("/accounts") as typeof state.accounts;
    if (result && "model_details" in result && result.model_details != null) {
      const match = state.accounts.find((a: { id: number }) => a.id === accountId);
      if (match) {
        (match as unknown as Record<string, unknown>)["quota_model_details"] = result.model_details;
      }
    }
    requestUpdate();
  } catch (err: unknown) {
    if (btn) flashButton(btn, "✗", "#f38ba8");
    const msg = err instanceof Error ? err.message : String(err);
    setTimeout(() => showToast("Error: " + msg, "error"), 100);
  } finally {
    if (btn) {
      setTimeout(() => { btn.disabled = false; btn.textContent = oldText; }, 1500);
    }
  }
}

async function onRefreshAllQuotas(providerId: string): Promise<void> {
  const accounts = (state.accounts || []).filter((a) => a.provider_id === providerId);
  const supported = accounts.filter((a) => {
    const p = state.providers.find((p) => p.id === a.provider_id);
    return p?.metadata?.supports_quota === true;
  });
  if (supported.length === 0) {
    showToast(`No accounts with quota support for ${providerId}.`, "info");
    return;
  }
  if (!confirm(`Refresh quota for ${supported.length} accounts?`)) return;
  for (const a of supported) {
    try {
      await api("/accounts/" + a.id + "/refresh-quota", { method: "POST" });
    } catch (err: unknown) {
      console.error("Failed to refresh quota for", a.id, err);
    }
  }
  state.accounts = await api("/accounts") as typeof state.accounts;
  requestUpdate();
  showToast("Quotas refreshed.", "success");
}

function onShowCreateAccount(providerId: string): void { showCreateAccount(providerId); }
function onShowUpdateAccountKey(id: number): void { showUpdateAccountKey(id); }

async function onDeleteAccount(id: number): Promise<void> {
  try {
    await api("/accounts/" + id, { method: "DELETE" });
    state.accounts = state.accounts.filter((a) => a.id !== id);
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

// ---- Handlers: models section ----

// THE BULK TOGGLE — the lag the operator reported. The previous
// implementation patched each row in place via `syncModelRowActive`
// after re-fetching the whole /models cache. With lit-html we just
// update `state.models` and call `requestUpdate()`; lit-html diffs
// the table against the previous render and patches only the rows
// whose `active` flag actually changed.
async function onBulkToggleModels(providerId: string, active: boolean): Promise<void> {
  const models = (state.models || []).filter((m) => m.provider_id === providerId);
  const customCount = models.filter((m) => m.custom).length;
  const toToggleCount = models.filter((m) => !m.custom && m.active !== active).length;
  if (toToggleCount === 0) {
    showToast("Nothing to toggle.", "info");
    return;
  }
  const msg = active
    ? `Enable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`
    : `Disable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`;
  if (!confirm(msg)) return;
  try {
    await api("/models/bulk-toggle", {
      method: "POST",
      body: JSON.stringify({ provider_id: providerId, active }),
    });
    state.models = await api("/models") as typeof state.models;
    requestUpdate();
  } catch (err: unknown) {
    const msg2 = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg2, "error");
  }
}

function onShowCustomModelForm(providerId: string): void { showCustomModelForm(providerId); }

async function onUpdateAutoActivate(providerId: string, e: Event | null): Promise<void> {
  // Only fire on "change" (blur/enter), not on every "input"
  // keystroke — same guard as the combos view's number inputs.
  if (e && e.type === "input") return;
  const target = e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const value = target ? target.value : "";
  const body = { auto_activate_keyword: value && value.trim() ? value.trim() : null };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    });
    state.providers = await api("/providers") as typeof state.providers;
    // No requestUpdate() — the input is uncontrolled; re-rendering
    // would close any other open input on the page.
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

async function onUpdateUseProxies(providerId: string, e: Event): Promise<void> {
  const target = e.target instanceof HTMLInputElement ? e.target : null;
  if (!target) return;
  const value = target.checked;
  const body = { use_proxies: value };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    });
    state.providers = await api("/providers") as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

async function onUpdateProxyRotationErrors(providerId: string, e: Event): Promise<void> {
  if (e.type === "input") return;
  const target = e.target instanceof HTMLInputElement ? e.target : null;
  if (!target) return;
  const value = target.value.trim();
  const body = { proxy_rotation_errors: value ? value : "429,connect_error,timeout" };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    });
    state.providers = await api("/providers") as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

// Update the per-provider search/filter state and re-render via
// requestUpdate(). lit-html diffs the tbody against the previous
// render — the search input keeps focus because it lives outside
// the tbody and is never replaced.
function onUpdateProviderSearch(providerId: string, e: Event): void {
  const target = e.target;
  const value = target instanceof HTMLInputElement ? target.value : "";
  const ui = getProviderUi(providerId);
  ui.search = value;
  setProviderUi(providerId, ui);
  requestUpdate();
}

function onUpdateProviderFilter(providerId: string, filter: "all" | "active" | "inactive"): void {
  const ui = getProviderUi(providerId);
  ui.filter = filter;
  setProviderUi(providerId, ui);
  requestUpdate();
}

function onCycleProviderSort(providerId: string, sortKey: string): void {
  const ui = getProviderUi(providerId);
  const current = ui.sort;
  let next: ModelSort | null = null;
  if (!current || current.key !== sortKey) {
    next = { key: sortKey, dir: "asc" };
  } else if (current.dir === "asc") {
    next = { key: sortKey, dir: "desc" };
  } else {
    next = null;
  }
  ui.sort = next;
  setProviderUi(providerId, ui);
  requestUpdate();
}

// Selection (multi-select) — toggle the row_id in state.selectedModels
// and re-render. lit-html diffs the tbody so the row's `selected`
// class + the bulk-actions bar update in place.
function onToggleModelSelection(rowId: number, e: Event | null): void {
  const target = e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  if (checked) state.selectedModels.add(rowId);
  else state.selectedModels.delete(rowId);
  requestUpdate();
}

function onToggleSelectAllModels(e: Event | null): void {
  const target = e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  if (!detailProviderId) return;
  const ui = getProviderUi(detailProviderId);
  const searchLower = (ui.search || "").toLowerCase();
  const visible = (state.models || [])
    .filter((m) => m.provider_id === detailProviderId)
    .filter((m) => {
      if (ui.filter === "active" && !m.active) return false;
      if (ui.filter === "inactive" && m.active) return false;
      if (searchLower && !m.model_id.toLowerCase().includes(searchLower)) return false;
      return true;
    })
    .map((m) => m.row_id);
  if (checked) {
    for (const id of visible) state.selectedModels.add(id);
  } else {
    for (const id of visible) state.selectedModels.delete(id);
  }
  requestUpdate();
}

function onClearModelSelection(): void {
  state.selectedModels.clear();
  requestUpdate();
}

async function onBulkSetSelected(providerId: string, active: boolean): Promise<void> {
  void providerId; // kept for handler-shape parity with the other bulk actions
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  if (!confirm(`${active ? "Enable" : "Disable"} ${ids.length} models?`)) return;
  try {
    await Promise.all(ids.map((rowId) =>
      api("/models/" + rowId + "/toggle", {
        method: "POST",
        body: JSON.stringify({ active }),
      }).catch((err: unknown) => console.error("Failed toggle", rowId, err))
    ));
    state.models = await api("/models") as Model[];
    state.selectedModels.clear();
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

function onBulkEnableSelected(providerId: string): Promise<void> { return onBulkSetSelected(providerId, true); }
function onBulkDisableSelected(providerId: string): Promise<void> { return onBulkSetSelected(providerId, false); }

async function onBulkTestSelected(providerId: string): Promise<void> {
  void providerId; // providerId unused — kept for handler-shape parity
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  if (!confirm(`Test ${ids.length} models sequentially?`)) return;
  try {
    for (const rowId of ids) {
      const btn = document.getElementById(`test-btn-${rowId}`) as HTMLButtonElement | null;
      if (btn) {
        btn.disabled = true;
        btn.textContent = "Testing...";
      }

      const accountSelect = document.getElementById(`test-account-${rowId}`) as HTMLSelectElement | null;
      const proxySelect = document.getElementById(`test-proxy-${rowId}`) as HTMLSelectElement | null;
      const accountId = accountSelect && accountSelect.value ? parseInt(accountSelect.value, 10) : null;
      const proxyId = proxySelect && proxySelect.value ? proxySelect.value : null;

      const result = (await api(`/models/${rowId}/test`, {
        method: "POST",
        body: JSON.stringify({ account_id: accountId, proxy_id: proxyId }),
      })) as { status: number; elapsed_ms: number; row_id?: number };
      const m = (state.models || []).find((x) => x.row_id === rowId);
      if (m) {
        m.last_test_status = result.status;
        m.last_test_at = new Date().toISOString();
      }
      if (btn) {
        if (result.status >= 200 && result.status < 300) {
          btn.textContent = "✓";
          btn.style.background = "#a6e3a1";
        } else {
          btn.textContent = "✗ " + result.status;
          btn.style.background = "#f38ba8";
        }
        setTimeout(() => {
          btn.textContent = "Test";
          btn.style.background = "";
          btn.disabled = false;
        }, 1500);
      }
    }
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

async function onBulkDeleteSelected(providerId: string): Promise<void> {
  void providerId; // providerId unused — kept for handler-shape parity
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  if (!confirm(`Delete ${ids.length} models? This cannot be undone.`)) return;
  try {
    await Promise.all(ids.map((rowId) =>
      api("/models/" + rowId, { method: "DELETE" })
        .catch((err: unknown) => console.error("Failed delete", rowId, err))
    ));
    state.models = await api("/models") as Model[];
    state.selectedModels.clear();
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

async function onToggleModel(rowId: number, newActive: boolean): Promise<void> {
  try {
    await api("/models/" + rowId + "/toggle", {
      method: "POST",
      body: JSON.stringify({ active: newActive }),
    });
    const m = (state.models || []).find((x) => x.row_id === rowId);
    if (m) m.active = newActive;
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

async function onTestModel(rowId: number, e: Event | null): Promise<void> {
  const btn = (e && e.target instanceof HTMLButtonElement ? e.target : null) as HTMLButtonElement | null;
  if (!btn) return;
  const oldText = btn.textContent;
  btn.disabled = true;
  btn.textContent = "Testing...";

  const accountSelect = document.getElementById(`test-account-${rowId}`) as HTMLSelectElement | null;
  const proxySelect = document.getElementById(`test-proxy-${rowId}`) as HTMLSelectElement | null;
  const accountId = accountSelect && accountSelect.value ? parseInt(accountSelect.value, 10) : null;
  const proxyId = proxySelect && proxySelect.value ? proxySelect.value : null;

  try {
    const result = (await api(`/models/${rowId}/test`, {
      method: "POST",
      body: JSON.stringify({ account_id: accountId, proxy_id: proxyId }),
    })) as { status: number; elapsed_ms: number; row_id?: number };
    const rid = result.row_id ?? rowId;
    const m = (state.models || []).find((x) => x.row_id === rid);
    if (m) {
      m.last_test_status = result.status;
      m.last_test_at = new Date().toISOString();
    }
    if (result.status >= 200 && result.status < 300) {
      flashButton(btn, "✓", "#a6e3a1");
    } else if (result.status === 0) {
      flashButton(btn, "✗ net", "#f38ba8");
    } else {
      flashButton(btn, "✗ " + result.status, "#f38ba8");
    }
    requestUpdate();
  } catch (err: unknown) {
    flashButton(btn, "✗", "#f38ba8");
    const msg = err instanceof Error ? err.message : String(err);
    setTimeout(() => showToast("Test failed: " + msg, "error"), 100);
  } finally {
    setTimeout(() => {
      btn.disabled = false;
      btn.textContent = oldText;
    }, 1500);
  }
}

async function onDeleteModel(rowId: number): Promise<void> {
  if (!confirm("Delete this model? Combo targets referencing it will be removed too.")) return;
  try {
    await api(`/models/${rowId}`, { method: "DELETE" });
    state.models = state.models.filter((m) => m.row_id !== rowId);
    state.selectedModels.delete(rowId);
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

// ---- Templates: grid ----

function renderProviderCard(p: Provider, accounts: Account[]): TemplateResult {
  const unhealthyAccs = accounts.filter((a) => a.health_status === "unhealthy").length;
  const activeModels = p.active_models ?? 0;
  const totalModels = p.total_models ?? 0;
  const cardClasses: string = [
    "provider-card",
    unhealthyAccs > 0 ? "has-errors" : "",
    p.active ? "" : "inactive",
  ].filter(Boolean).join(" ");
  return html`<a href="#/providers/${encodeURIComponent(p.id)}" class=${cardClasses}>
    <div class="provider-card-header">
      <div class="provider-icon" data-format=${p.format}>${providerGlyph(p.id)}</div>
      <div class="provider-info">
        <h3>${p.name}${p.active ? html`` : html` <small class="inactive-suffix">(inactive)</small>`}</h3>
        <code>${p.id}</code>
      </div>
    </div>
    <div class="provider-card-body">
      <div class="capabilities">
        <span class="chip" data-format=${p.format}>${p.format}</span>
        <span class="chip">${p.auth_type}</span>
      </div>
    </div>
    <div class="provider-card-footer">
      <div class="stat">
        <label>Accounts</label>
        <value>${accounts.length}</value>
        ${unhealthyAccs > 0 ? html`<span class="badge badge-error">${unhealthyAccs} down</span>` : html``}
      </div>
      <div class="stat">
        <label>Models</label>
        <value>${activeModels}/${totalModels}</value>
      </div>
    </div>
  </a>`;
}

function renderProviderGrid(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><h2>Providers</h2>
        <div class="actions">
          <button @click=${onRefreshAllProviders}>Refresh all</button>
          <button class="primary" @click=${onShowCreateProvider}>+ Add provider</button>
        </div>
      </div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }
  const list = state.providers || [];
  const cards: TemplateResult = list.length === 0
    ? html`<div class="empty-state">
        <h3>No providers configured</h3>
        <p>Add a provider to get started.</p>
        <button class="primary" @click=${onShowCreateProvider}>+ Add provider</button>
      </div>`
    : html`<div class="provider-grid">${list.map((p) => {
        const accounts = (state.accounts || []).filter((a) => a.provider_id === p.id);
        return renderProviderCard(p, accounts);
      })}</div>`;
  return html`
    <div class="page-header"><h2>Providers</h2>
      <div class="actions">
        <button @click=${onRefreshAllProviders}>Refresh all</button>
        <button class="primary" @click=${onShowCreateProvider}>+ Add provider</button>
      </div>
    </div>
    ${cards}
  `;
}

// ---- Templates: detail ----

function renderDetailHeader(provider: Provider): TemplateResult {
  const isDeletable = provider.metadata?.deletable ?? true;
  return html`
    <div class="provider-detail-header${provider.active ? "" : " inactive"}">
      <div class="provider-icon icon-large" data-format=${provider.format}>${providerGlyph(provider.id)}</div>
      <div style="flex:1; min-width:0;">
        <h2>
          <span class="editable" title="Click to rename" @click=${() => onRenameProvider(provider.id, provider.name)}>${provider.name}</span>
          <small>✎</small>
        </h2>
        <code>${provider.id}</code>
        <div class="meta">
          <span class="chip" data-format=${provider.format}>${provider.format}</span>
          <span class="chip">${provider.auth_type}</span>
          <a href=${provider.base_url} target="_blank" rel="noopener" class="meta-link">${provider.base_url}</a>
          ${provider.active ? html`` : html`<span class="chip inactive-chip">inactive</span>`}
        </div>
      </div>
      <div class="actions">
        <button @click=${(e: Event) => onRefreshProvider(provider.id, e)}>↻ Refresh models</button>
        <button class="primary" @click=${() => onToggleProviderActive(provider.id, !provider.active)}>
          ${provider.active ? "Deactivate" : "Activate"}
        </button>
          ${!isDeletable
            ? html`<button class="locked" disabled title="Built-in providers cannot be deleted. Deactivate them instead.">🔒 Delete (built-in)</button>`
            : html`<button class="danger small" @click=${() => onConfirmDeleteProvider(provider.id)}>Delete</button>`}
      </div>
    </div>
  `;
}

function renderOAuthSection(provider: Provider): TemplateResult {
  if (provider.auth_type !== "oauth") return html``;
  const buttons: TemplateResult[] = [];
  if (provider.oauth_flows?.includes("pkce")) {
    buttons.push(html`<button class="primary" @click=${() => onOAuthStartPKCE(provider.id)}>Log in with ${provider.name || provider.id}</button>`);
  }
  if (provider.oauth_flows?.includes("device")) {
    buttons.push(html`<button class="primary" @click=${() => onOAuthStartDeviceCode(provider.id)}>Log in with ${provider.name || provider.id}</button>`);
  }
  return html`
    <section class="detail-section">
      <div class="section-header"><h3>OAuth login</h3></div>
      <div class="oauth-buttons">${buttons}</div>
      <div id="oauth-device-info" style="display:none;"></div>
      <div id="oauth-manual-section" class="oauth-manual-card" style="display:none;">
        <h4>1. Authorize</h4>
        <p>Open this URL in a new tab and complete the login:</p>
        <div class="oauth-manual-url">
          <input id="oauth-auth-url" type="text" readonly>
          <button type="button" class="btn-secondary" @click=${onCopyAuthUrl}>Copy</button>
        </div>
        <h4>2. Paste the callback URL</h4>
        <p>After the OAuth provider redirects, copy the full URL from your address bar and paste it here:</p>
        <div class="oauth-manual-input">
          <input id="oauth-callback-input" type="text" placeholder="https://...">
          <button type="button" class="primary" @click=${onOAuthSubmitManualCallback}>Submit</button>
        </div>
      </div>
    </section>
  `;
}

function renderConnectionsSection(provider: Provider, accounts: Account[]): TemplateResult {
  const hasQuota = provider.metadata?.supports_quota ?? false;
  const body: TemplateResult = accounts.length === 0
    ? html`<table><tbody><tr><td colspan="6" class="empty-row">No accounts. Add an API key to start using this provider.</td></tr></tbody></table>`
    : html`<table>
        <thead><tr><th>Label</th><th>Priority</th><th>Health</th><th>Quota</th><th>Created</th><th>Actions</th></tr></thead>
        <tbody>${accounts.map((a) => {
          const quotaCell: TemplateResult = hasQuota
            ? html`<td>${renderQuotaCell(a)}</td>`
            : html`<td><div class="quota-cell muted"><small>not supported by this provider</small></div></td>`;
          return html`<tr>
            <td>${a.label || a.email || "—"}</td>
            <td>${a.priority}</td>
            <td>
              <select class=${"health-select " + (a.health_status || "unknown")} @change=${(e: Event) => onSetHealth(a.id, e)}>
                <option value="healthy" ?selected=${a.health_status === "healthy"}>healthy</option>
                <option value="degraded" ?selected=${a.health_status === "degraded"}>degraded</option>
                <option value="unhealthy" ?selected=${a.health_status === "unhealthy"}>unhealthy</option>
              </select>
            </td>
            ${quotaCell}
            <td>${a.created_at || "—"}</td>
            <td>
              ${hasQuota ? html`<button class="small" @click=${(e: Event) => onRefreshAccountQuota(a.id, e)}>↻ Quota</button>` : html``}
              <button class="small" @click=${() => onShowUpdateAccountKey(a.id)}>🔑 Key</button>
              <button class="small danger" @click=${() => onDeleteAccount(a.id)}>Delete</button>
            </td>
          </tr>`;
        })}</tbody>
      </table>`;
  const toolbar: TemplateResult = html`<div>
    ${hasQuota ? html`<button @click=${() => onRefreshAllQuotas(provider.id)}>↻ Refresh all quotas</button>` : html``}
    <button class="primary" @click=${() => onShowCreateAccount(provider.id)}>+ Add account</button>
  </div>`;
  return html`<section class="detail-section">
    <div class="section-header"><h3>Connections (${accounts.length})</h3>${toolbar}</div>
    ${body}
  </section>`;
}

function renderModelsSection(provider: Provider, providerModels: Model[], ui: ProviderDetailUiState): TemplateResult {
  const activeModels = providerModels.filter((m) => m.active).length;
  const searchLower = (ui.search || "").toLowerCase();
  const filtered = providerModels.filter((m) => {
    if (ui.filter === "active" && !m.active) return false;
    if (ui.filter === "inactive" && m.active) return false;
    if (searchLower && !m.model_id.toLowerCase().includes(searchLower)) return false;
    return true;
  });
  const sorted = applySort(filtered, ui.sort);
  // Compute the master "select all" checkbox state from the visible
  // rows so a re-render (filter change, sort, poll) keeps it in sync
  // with reality — checked if all visible rows are selected,
  // indeterminate if only some.
  const visibleRowIds: number[] = sorted.map((m) => m.row_id);
  const selectedVisible: number = visibleRowIds.filter((id) => (state.selectedModels as Set<number>).has(id)).length;
  const allSelected: boolean = visibleRowIds.length > 0 && selectedVisible === visibleRowIds.length;
  const indeterminate: boolean = selectedVisible > 0 && !allSelected;
  const bulkBar: TemplateResult = state.selectedModels.size > 0
    ? html`<div class="bulk-actions-bar">
        <span><strong>${state.selectedModels.size}</strong> selected</span>
        <button @click=${() => onBulkEnableSelected(provider.id)}>Enable selected</button>
        <button @click=${() => onBulkDisableSelected(provider.id)}>Disable selected</button>
        <button @click=${() => onBulkTestSelected(provider.id)}>Test selected</button>
        <button class="danger" @click=${() => onBulkDeleteSelected(provider.id)}>Delete selected</button>
        <button class="link" @click=${onClearModelSelection}>Clear selection</button>
      </div>`
    : html``;
  return html`
    <section class="detail-section">
      <div class="section-header">
        <h3>Models (${activeModels}/${providerModels.length} active)</h3>
        <div>
          <button @click=${() => onBulkToggleModels(provider.id, true)}>Enable all</button>
          <button @click=${() => onBulkToggleModels(provider.id, false)}>Disable all</button>
          <button class="primary" @click=${() => onShowCustomModelForm(provider.id)}>+ Custom model</button>
        </div>
      </div>

      <div class="auto-activate-bar">
        <label>
          Auto-activate on refresh:
          <input type="text"
                 placeholder="(empty = enable all)"
                 .value=${provider.auto_activate_keyword || ""}
                 @change=${(e: Event) => onUpdateAutoActivate(provider.id, e)}
                 @input=${(e: Event) => onUpdateAutoActivate(provider.id, e)}>
        </label>
        <small>Models whose ID contains this string are auto-enabled on refresh. Empty = enable all new models.</small>
      </div>

      <div class="auto-activate-bar" style="margin-top: 1rem; display: flex; gap: 2rem; align-items: center; flex-wrap: wrap;">
        <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal; cursor: pointer;">
          <input type="checkbox"
                 .checked=${!!provider.use_proxies}
                 @change=${(e: Event) => onUpdateUseProxies(provider.id, e)}>
          Use proxies for this provider
        </label>
        ${provider.use_proxies ? html`
          <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal; flex: 1;">
            Rotate proxy on errors:
            <input type="text"
                   style="flex: 1; max-width: 300px; padding: 0.25rem 0.5rem; font-size: var(--fs-sm); border: var(--border-w) var(--border-style) var(--color-border); border-radius: var(--radius-sm); background: var(--color-surface); color: var(--color-text);"
                   placeholder="429,connect_error,timeout"
                   .value=${provider.proxy_rotation_errors || "429,connect_error,timeout"}
                   @change=${(e: Event) => onUpdateProxyRotationErrors(provider.id, e)}
                   @input=${(e: Event) => onUpdateProxyRotationErrors(provider.id, e)}>
          </label>
          ${provider.current_proxy_id ? html`
            <span style="font-size: var(--fs-sm); color: var(--color-text-muted); background: var(--color-surface-soft); padding: 0.25rem 0.5rem; border-radius: var(--radius-sm);">
              Bound Proxy: <code>${provider.current_proxy_id}</code>
            </span>
          ` : html`
            <span style="font-size: var(--fs-sm); color: var(--color-warn); background: var(--color-warn-soft); padding: 0.25rem 0.5rem; border-radius: var(--radius-sm);">
              No active proxy bound
            </span>
          `}
        ` : html``}
      </div>

      <div class="filter-bar">
        <input type="text" placeholder="Search models..." .value=${ui.search || ""}
               @input=${(e: Event) => onUpdateProviderSearch(provider.id, e)}>
        <div class="filter-tabs">
          <button class=${"filter-tab " + (ui.filter === "all" ? "active" : "")} @click=${() => onUpdateProviderFilter(provider.id, "all")}>All (${providerModels.length})</button>
          <button class=${"filter-tab " + (ui.filter === "active" ? "active" : "")} @click=${() => onUpdateProviderFilter(provider.id, "active")}>Active (${activeModels})</button>
          <button class=${"filter-tab " + (ui.filter === "inactive" ? "active" : "")} @click=${() => onUpdateProviderFilter(provider.id, "inactive")}>Inactive (${providerModels.length - activeModels})</button>
        </div>
      </div>

      ${bulkBar}

      <table>
        <thead><tr>
          <th><input type="checkbox" .checked=${allSelected} .indeterminate=${indeterminate} @change=${(e: Event) => onToggleSelectAllModels(e)}></th>
          ${SORTABLE_COLUMNS.map((c: SortableColumn) => renderSortableTh(c, ui.sort, provider.id))}
          <th>Capabilities</th><th>Status</th><th>Last test</th><th>Actions</th>
        </tr></thead>
        <tbody id="models-tbody">
          ${sorted.length === 0
            ? html`<tr><td colspan="10" class="empty-row">No models match the filter.</td></tr>`
            : html`${sorted.map((m: Model) => renderModelRow(m))}`}
        </tbody>
      </table>
    </section>
  `;
}

function renderSortableTh(col: SortableColumn, sort: ModelSort | null, providerId: string): TemplateResult {
  const isActive = !!(sort && sort.key === col.key);
  const indicator: string = isActive ? (sort && sort.dir === "desc" ? " ▼" : " ▲") : "";
  return html`<th class=${"sortable" + (isActive ? " sorted" : "")} @click=${() => onCycleProviderSort(providerId, col.key)}>${col.label}<span class="sort-indicator">${indicator}</span></th>`;
}

function renderModelRow(m: Model): TemplateResult {
  const isSelected: boolean = (state.selectedModels as Set<number>).has(m.row_id);
  const lastTest: TemplateResult = m.last_test_status != null
    ? html`<span class=${"status-pill " + statusPillClass(m.last_test_status)}>${String(m.last_test_status)}</span> <small>${m.last_test_at || ""}</small>`
    : html`<span class="muted">never</span>`;

  const providerAccounts = (state.accounts || []).filter((a) => a.provider_id === m.provider_id);
  const aliveProxies = (state.proxies || []).filter((p) => p.status === "alive");

  return html`<tr id=${`model-row-${m.row_id}`} class=${(m.active ? "" : "inactive") + (isSelected ? " selected" : "")}>
    <td><input type="checkbox" ?checked=${isSelected} @change=${(e: Event) => onToggleModelSelection(m.row_id, e)}></td>
    <td><code>${m.model_id}</code>${m.custom ? html`<span class="badge custom">custom</span>` : html``}</td>
    <td>${m.display_name || "—"}</td>
    <td>${m.target_format || "—"}</td>
    <td>${formatContext(m.context_length)}</td>
    <td>${formatContext(m.max_output_tokens)}</td>
    <td>${renderCapabilityBadges(m.capabilities_json)}${m.family ? html` <small class="muted">${m.family}</small>` : html``}</td>
    <td><span class=${"status-pill " + (m.active ? "on" : "off")}>${m.active ? "active" : "inactive"}</span></td>
    <td class="last-test-cell">${lastTest}</td>
    <td>
      <div style="display: flex; flex-direction: column; gap: 4px; margin-bottom: 6px;">
        <select id=${`test-account-${m.row_id}`} style="padding: 2px 4px; font-size: 11px; background: var(--color-surface); color: var(--color-text); border: 1px solid var(--color-border); border-radius: 4px; max-width: 140px;">
          <option value="">(Default Account)</option>
          ${providerAccounts.map((a) => html`<option value=${a.id}>${a.label || `Account #${a.id}`}</option>`)}
        </select>
        <select id=${`test-proxy-${m.row_id}`} style="padding: 2px 4px; font-size: 11px; background: var(--color-surface); color: var(--color-text); border: 1px solid var(--color-border); border-radius: 4px; max-width: 140px;">
          <option value="">(No Proxy)</option>
          ${aliveProxies.map((p) => html`<option value=${p.id}>${p.host}:${p.port} (${p.latency_ms || '?'}ms)</option>`)}
        </select>
      </div>
      <div style="display: flex; gap: 4px; align-items: center;">
        <button class="small" id=${`test-btn-${m.row_id}`} @click=${(e: Event) => onTestModel(m.row_id, e)}>Test</button>
        <button class="small" @click=${() => onToggleModel(m.row_id, !m.active)}>${m.active ? "Disable" : "Enable"}</button>
        <button class="small danger" @click=${() => onDeleteModel(m.row_id)}>×</button>
      </div>
    </td>
  </tr>`;
}

function renderProviderDetail(): TemplateResult {
  if (loadError) {
    return html`<div class="banner banner-error">${loadError}</div>`;
  }
  if (!detailProviderId) return html`<div class="loading">Loading...</div>`;
  const provider = (state.providers || []).find((p) => p.id === detailProviderId);
  if (!provider) {
    // If state.providers is empty, the fetch is still in progress —
    // show "Loading..." instead of "not found". The "not found" error
    // only shows after the fetch completes and the provider still
    // isn't in the list. This fixes the "phantom provider" issue
    // where navigating from a notification's "View provider" button
    // showed an empty provider page because the data hadn't loaded yet.
    if ((state.providers || []).length === 0) {
      return html`<div class="loading">Loading provider...</div>`;
    }
    return html`<div class="banner banner-error">Provider ${detailProviderId} not found. <a href="#/providers">← Back</a></div>`;
  }
  const accounts = (state.accounts || []).filter((a) => a.provider_id === detailProviderId);
  const providerModels = (state.models || []).filter((m) => m.provider_id === detailProviderId);
  // Per-provider UI state. Default to "all" / empty search on first
  // visit; keep the user's previous selection on subsequent visits.
  if (!state.providerDetail[detailProviderId]) {
    setProviderUi(detailProviderId, { filter: "all", search: "", sort: null });
  } else if ((state.providerDetail[detailProviderId] as Partial<ProviderDetailUiState>).sort === undefined) {
    const existing = state.providerDetail[detailProviderId] as Partial<ProviderDetailUiState>;
    setProviderUi(detailProviderId, { filter: existing.filter ?? "all", search: existing.search ?? "", sort: null });
  }
  const ui = getProviderUi(detailProviderId);
  return html`
    <div class="page-header"><a href="#/providers" class="back-link">← All providers</a><h2>${provider.name}</h2></div>
    ${renderDetailHeader(provider)}
    ${renderOAuthSection(provider)}
    ${renderConnectionsSection(provider, accounts)}
    ${renderModelsSection(provider, providerModels, ui)}
  `;
}

// ---- Mount ----

export interface MountProvidersOpts {
  detailId?: string;
}

export async function mountProviders(opts: MountProvidersOpts = {}): Promise<(() => void) | void> {
  const main = document.getElementById("main");
  if (!main) return;

  if (opts.detailId) {
    detailProviderId = opts.detailId;
    loadError = null;
    // Switching providers always starts with an empty selection —
    // visible row_ids live in the previous provider's table.
    if (state.selectedModelsProvider !== opts.detailId) {
      state.selectedModels.clear();
      state.selectedModelsProvider = opts.detailId;
    }
    const cleanup = mountView(main, renderProviderDetail);
    try {
      // Cold paint: fetch providers/accounts/models. Warm re-render
      // (from cache after navigate()) skips the network initially, but
      // does a background refresh to prevent frozen quotas and statuses.
      const proxiesPromise = api("/proxies?status=alive") as Promise<any[]>;
      if (state.providers.length === 0) {
        const [providers, accounts, models, proxies] = await Promise.all([
          api("/providers") as Promise<Provider[]>,
          api("/accounts") as Promise<Account[]>,
          api("/models") as Promise<Model[]>,
          proxiesPromise,
        ]);
        state.providers = providers;
        state.accounts = accounts;
        state.models = models;
        state.proxies = proxies;
      } else {
        state.proxies = await proxiesPromise;
        Promise.all([
          api("/providers") as Promise<Provider[]>,
          api("/accounts") as Promise<Account[]>,
          api("/models") as Promise<Model[]>,
        ]).then(([p, a, m]) => {
          state.providers = p;
          state.accounts = a;
          state.models = m;
          requestUpdate();
        }).catch((e) => console.error("Background refresh failed:", e));
      }
      requestUpdate();
    } catch (e: unknown) {
      loadError = e instanceof Error ? e.message : String(e);
      requestUpdate();
    }
    return cleanup;
  }

  // Grid view.
  detailProviderId = null;
  loadError = null;
  const cleanup = mountView(main, renderProviderGrid);
  try {
    const hasCache = state.providers && state.providers.length > 0;
    const [providers, accounts, proxies] = await Promise.all([
      hasCache ? Promise.resolve(state.providers) : api("/providers") as Promise<Provider[]>,
      hasCache && state.accounts ? Promise.resolve(state.accounts) : api("/accounts") as Promise<Account[]>,
      api("/proxies?status=alive") as Promise<any[]>,
    ]);
    state.providers = providers;
    state.accounts = accounts;
    state.proxies = proxies;
    requestUpdate();

    if (hasCache) {
      Promise.all([
        api("/providers") as Promise<Provider[]>,
        api("/accounts") as Promise<Account[]>,
      ]).then(([p, a]) => {
        state.providers = p;
        state.accounts = a;
        requestUpdate();
      }).catch((e) => console.error("Background refresh failed:", e));
    }
  } catch (e: unknown) {
    loadError = e instanceof Error ? e.message : String(e);
    requestUpdate();
  }
  return cleanup;
}
