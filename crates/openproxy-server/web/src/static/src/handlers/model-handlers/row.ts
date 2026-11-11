// handlers/model-handlers/row.ts — per-row model handlers, multi-select management and the
// shared bulk-bar DOM patch. `updateBulkBar` and the `TestResult` interface are exported from
// here because both this module and bulk.ts reference them.

import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { html, render } from "lit-html";
import {
  renderModelRows,
  getVisibleModelRowIds,
  updateFilterTabCounts,
  syncSelectAllCheckbox,
  applySort,
  syncModelRowActive,
} from "../../components/model-table.js";
import { renderBulkActionsBar } from "../../components/model-bulk-actions.js";
import { statusPillClass } from "../../lib/constants.js";
import { requestUpdate } from "../../state/reactive.js";
import { flashButton, showApiError } from "../../lib/ui-utils.js";
import { beginButtonBusy, resolveBusyButton } from "../../lib/button-busy.js";

export interface TestResult {
  status: number;
  elapsed_ms: number;
  row_id?: number;
}

// ── Per-row model handlers ─────────────────────────────────────────────────

// Soft-disable / re-enable a model. `rowId` is the server-side numeric primary key, NOT the
// upstream model id.
export async function toggleModel(rowId: number, newActive: boolean | unknown, e: Event | null): Promise<void> {
  // The data-action shim passes the event last. `newActive` is a boolean from data-arg2, or
  // falls back to the event target's checked state.
  const desired: boolean = typeof newActive === "boolean" ? newActive
    : !!(e && e.target && e.target instanceof HTMLInputElement ? e.target.checked : false);
  try {
    await api("/models/" + rowId + "/toggle", {
      method: "POST",
      body: JSON.stringify({ active: desired }),
    });
    const m = (state.models || []).find((x) => x.row_id === rowId);
    if (m) m.active = desired;
    // Targeted DOM patch of the row's active-state UI (row class, status pill, Enable/Disable
    // button). No requestUpdate(): a full rebuild would close an open <select> (filter tabs,
    // provider dropdown) and steal focus from the search input. Mirrors patchComboField.
    syncModelRowActive(rowId, desired);
    // Refresh the (All / Active / Inactive) counts on the filter tabs.
    const ctx = state.currentView && state.currentView.context;
    if (ctx) {
      const allProviderModels = (state.models || []).filter((mm) => mm.provider_id === ctx);
      updateFilterTabCounts(ctx, allProviderModels);
    }
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

// Fire a single upstream test for one model. Only the affected row's "last test" cell is
// repainted (a full redraw is wasteful on a 200-row table), and the button flashes so the click
// feels acknowledged on a slow request. The busy state is resolved before the first await and
// released in `finally`, so a network error can never leave the button stuck on "Testing…".
export async function testModel(rowId: number, _modelId: string, e: Event | null): Promise<void> {
  const btn = resolveBusyButton(e) ?? (document.getElementById(`test-btn-${rowId}`) as HTMLButtonElement | null);
  const originalHtml = btn ? btn.innerHTML : "";
  const busy = beginButtonBusy(btn, { label: "Testing…" });
  let flash: { text: string; color: string } | null = null;
  try {
    const result = (await api(`/models/${rowId}/test`, { method: "POST" })) as TestResult;
    // Repaint only the "last test" cell to preserve scroll/focus. The server sets the row id in
    // its response; fall back to the request rowId if it omits it (older builds).
    const rid = result.row_id ?? rowId;
    const row = document.getElementById(`model-row-${rid}`);
    if (row) {
      // Located by class, not index: the "Last test" cell's position shifts with column reorders.
      const cell = row.querySelector(".last-test-cell");
      if (cell instanceof HTMLElement) {
        render(html`<span class="status-pill ${statusPillClass(result.status)}">${result.status}</span> <small>${result.elapsed_ms}ms</small>`, cell);
      }
    }
    if (result.status >= 200 && result.status < 300) {
      flash = { text: "✓", color: "#a6e3a1" };
    } else if (result.status === 0) {
      flash = { text: "✗ net", color: "#f38ba8" };
    } else {
      flash = { text: "✗ " + result.status, color: "#f38ba8" };
    }
  } catch (err: unknown) {
    flash = { text: "✗", color: "#f38ba8" };
    setTimeout(() => showApiError(err, "Test failed"), 100);
  } finally {
    busy.release();
    if (flash && btn) {
      flashButton(btn, flash.text, flash.color);
      btn.disabled = true;
      setTimeout(() => {
        btn.disabled = false;
        if (btn.isConnected) btn.innerHTML = originalHtml;
      }, 1500);
    }
  }
}

// ── Selection (multi-select) ───────────────────────────────────────────────
// A Set of model row_ids, cleared on provider navigation.

export function toggleModelSelection(rowId: number, e: Event | null): void {
  const target = e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  if (checked) state.selectedModels.add(rowId);
  else state.selectedModels.delete(rowId);
  // No full re-render: toggle the row's `selected` class and update the bulk bar in O(1).
  const row = document.getElementById(`model-row-${rowId}`);
  if (row) row.classList.toggle("selected", checked);
  updateBulkBar();
  // Sync the master "select all" checkbox from the visible row_ids (the DOM write is cheap).
  const visible = getVisibleModelRowIds();
  syncSelectAllCheckbox(visible);
}

// Toggle every row passing the active/inactive filter + search box, not every model of the
// provider: a 200-row provider where 3 rows match the search must not select 197 extras.
export function toggleSelectAllModels(e: Event | null): void {
  const target = e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  const visible = getVisibleModelRowIds();
  if (checked) {
    for (const id of visible) state.selectedModels.add(id);
  } else {
    for (const id of visible) state.selectedModels.delete(id);
  }
  // Targeted DOM patch: toggle each visible row's `selected` class and refresh the bulk bar.
  // No requestUpdate() — a full rebuild would close an open <select> and steal focus from the
  // search input. Mirrors patchComboField.
  for (const id of visible) {
    const row = document.getElementById(`model-row-${id}`);
    if (row) row.classList.toggle("selected", checked);
  }
  updateBulkBar();
}

export function clearModelSelection(): void {
  state.selectedModels.clear();
  // Targeted DOM patch: uncheck every visible checkbox, drop every row's `selected` class,
  // hide the bulk bar and reset the master checkbox, preserving the rest of the DOM (a full
  // re-render would close an open <select> and steal search focus). Mirrors patchComboField.
  document.querySelectorAll<HTMLInputElement>(
    '#models-tbody input[type="checkbox"]'
  ).forEach((cb) => { cb.checked = false; });
  document.querySelectorAll("tr[id^='model-row-'].selected").forEach((row) => {
    row.classList.remove("selected");
  });
  updateBulkBar();
  syncSelectAllCheckbox([]);
}

// Re-render the bulk bar for the current count — cheaper than a full re-render: only the
// "N selected" counter changes, the bar's buttons stay intact.
export function updateBulkBar(): void {
  const tbody = document.getElementById("models-tbody");
  if (!tbody) return;
  const section = tbody.closest("section");
  if (!section) return;
  // The bar shares a <section> with the tbody. Update the count in place, or insert it
  // before the table on first paint; always re-query so no stale reference survives a re-render.
  let bar = section.querySelector(".bulk-actions-bar") as HTMLElement | null;
  const count = state.selectedModels.size;
  if (count === 0) {
    if (bar) {
      // Remove the bar plus any wrapper <div> this function inserted earlier. When the parent
      // view rendered the bar inline, parentElement is the <section> itself — leave that alone.
      const wrapper = bar.parentElement;
      bar.remove();
      if (wrapper && wrapper !== section && wrapper.children.length === 0) {
        wrapper.remove();
      }
    }
    return;
  }
  // Pull the provider id from the current view context.
  const providerId = state.currentView && state.currentView.context;
  if (!providerId) return;
  if (bar) {
    const strong = bar.querySelector("strong");
    if (strong) strong.textContent = String(count);
  } else {
    // Render the bar TemplateResult into a fresh wrapper <div> before the table; lit-html
    // replaces the wrapper's children with the bar markup.
    const table = section.querySelector("table");
    if (table) {
      const wrapper = document.createElement("div");
      table.insertAdjacentElement("beforebegin", wrapper);
      render(renderBulkActionsBar(providerId), wrapper);
    }
  }
}

// ── Filter / search ────────────────────────────────────────────────────────

// Update the per-provider search/filter state and repaint only the affected parts (the model
// tbody + the filter-tab counts). A full renderProviderDetail re-render would replace the search
// input and steal focus mid-keystroke, so the surrounding DOM stays stable and the tbody is
// patched in place — the search input keeps focus because it is never removed.
//
// Argument order: the shim passes data-arg-N then the event. The search input declares data-arg1
// (provider id) and data-arg2 (state key — "search" or "filter"); the new value comes from
// e.target.value (the live value of the input being edited). Filter tabs read data-arg3.
export function updateProviderFilter(providerId: string, key: string, valueFromArg3: unknown, event: Event | null): void {
  // The shim passes positional data-args then the event last. The search input has no
  // data-arg3, so the 3rd arg is the event itself; for the filter tabs data-arg3 holds the value
  // ("all" / "active" / "inactive"). Branch on whether the 3rd arg looks like an Event.
  let value: string;
  if (key === "filter") {
    // Filter tab: value comes from data-arg3 ("all", "active", "inactive").
    if (typeof valueFromArg3 === "string") {
      value = valueFromArg3;
    } else {
      const ev = event;
      const target = ev && ev.target ? ev.target : null;
      const closest = target instanceof Element ? target.closest("[data-action]") : null;
      const ds = closest instanceof HTMLElement ? closest.dataset : undefined;
      value = (ds && typeof ds["arg3"] === "string") ? ds["arg3"] : "all";
    }
  } else if (key === "search") {
    // Search input: the value is the live text in the input.
    const v3 = (valueFromArg3 && typeof valueFromArg3 === "object" && "target" in (valueFromArg3 as Record<string, unknown>))
      ? (valueFromArg3 as { target: EventTarget | null }).target
      : (event && event.target) || null;
    value = v3 && "value" in v3 ? String((v3 as { value: string }).value) : "";
  } else {
    value = "";
  }
  if (!state.providerDetail[providerId]) {
    state.providerDetail[providerId] = { filter: "all", search: "", sort: null };
  } else if (state.providerDetail[providerId]["sort"] === undefined) {
    // Backfill `sort` for providers visited before the sortable-headers feature landed.
    state.providerDetail[providerId]["sort"] = null;
  }
  state.providerDetail[providerId][key] = value;
  const ui = state.providerDetail[providerId] as { filter: string; search: string; sort: unknown };

  // Recompute the visible models with the same rules as renderProviderDetail. A shared
  // filterModels() helper would be three conditions of indirection; the duplication is clearer.
  const searchLower = (ui.search || "").toLowerCase();
  const allProviderModels = (state.models || []).filter((m) => m.provider_id === providerId);
  const filtered = allProviderModels.filter((m) => {
    if (ui.filter === "active" && !m.active) return false;
    if (ui.filter === "inactive" && m.active) return false;
    if (searchLower && !m.model_id.toLowerCase().includes(searchLower)) return false;
    return true;
  });
  // Apply the same sort the full render uses, so filtering doesn't reset the chosen ordering.
  const sorted = applySort(filtered, ui.sort as Parameters<typeof applySort>[1]);

  // Repaint the tbody and its empty-state row without touching the page chrome; the search
  // input lives outside the tbody so its focus survives, and lit-html diffs the TemplateResult so
  // only changed rows are patched.
  const tbody = document.getElementById("models-tbody");
  if (tbody) {
    render(
      sorted.length === 0
        ? html`<tr><td colspan="10" class="empty-row">No models match the filter.</td></tr>`
        : renderModelRows(sorted),
      tbody
    );
  }

  // Refresh the (All / Active / Inactive) counts via a single updater, so a future data-shape
  // change has one place to update.
  updateFilterTabCounts(providerId, allProviderModels);

  // The master "select all" state depends on which rows are visible (see the note in
  // renderProviderDetail). The full re-render deferred this to a microtask; a partial paint must
  // run it now since that queue won't be flushed.
  syncSelectAllCheckbox(sorted.map((m) => m.row_id));
}

// Persist the provider's auto-activate keyword. `change` fires once (blur/click-away) while the
// data-action dispatcher also fires per `input` keystroke, so `e.type === "input"` is filtered
// out to avoid a PATCH per key. The endpoint takes a three-state `null` / string: `null` clears
// the column back to NULL so a future refresh re-enables *all* non-custom models.
export async function updateAutoActivate(providerId: string, e: Event | null): Promise<void> {
  // Only fire on "change" (blur/enter), not per "input" keystroke — same guard as
  // updateTargetWeight etc.
  if (e && e.type === "input") return;
  const target = e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const value = target ? target.value : "";
  const body = { auto_activate_keyword: value && value.trim() ? value.trim() : null };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    });
    // Refresh the providers cache so the next background-poll diff is a no-op and the input
    // shows the server's normalized string.
    state.providers = await api("/providers") as typeof state.providers;
  } catch (err: unknown) {
    showApiError(err, "Error");
    // No re-render on error — see patchComboField: the user's text is already in the input
    // and a re-render would close other open inputs and steal focus.
  }
}

// Cycle the sort state for a models-table column. Signature matches the
// `data-action="cycleProviderSort"` shim: arg1 = providerId, arg2 = sortKey, then the event.
//
//   no sort          → sort by this column, asc
//   this column asc  → this column desc
//   this column desc → no sort (back to upstream order)
//
// A full re-render is required (not just a tbody paint) because the <th> indicators flip too and
// the partial-paint helper only re-renders rows.
export function cycleProviderSort(providerId: string, sortKey: string, _event: Event | null): void {
  if (!state.providerDetail[providerId]) {
    state.providerDetail[providerId] = { filter: "all", search: "", sort: null };
  }
  const current = state.providerDetail[providerId]["sort"] as { key: string; dir: string } | null | undefined;
  let next: { key: string; dir: string } | null = null;
  if (!current || current.key !== sortKey) {
    next = { key: sortKey, dir: "asc" };
  } else if (current.dir === "asc") {
    next = { key: sortKey, dir: "desc" };
  } else {
    next = null;
  }
  state.providerDetail[providerId]["sort"] = next;
  requestUpdate();
}
