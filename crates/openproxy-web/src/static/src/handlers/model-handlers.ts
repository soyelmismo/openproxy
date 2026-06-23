// handlers/model-handlers.ts — model-level handlers.
//
// Per spec §3 + §13.8 we do not attach to `window.*`. Every
// function here is exported by name and registered in
// handlers/registry.ts so the central data-action shim can find
// it.
//
// Naming convention: functions that take an `e` event as a
// trailing argument (submit handlers) receive the DOM event last
// in the shim dispatch. Functions that take a single `id`-style
// argument receive it as `arg1`. Functions that need a button
// reference (e.g. testModel) take the event element as a
// trailing argument so they can disable + relabel the button
// while in flight.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { html, render } from "lit-html";
import { escapeAttr } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";
import { renderModelRows, getVisibleModelRowIds, updateFilterTabCounts, syncSelectAllCheckbox, applySort, syncModelRowActive } from "../components/model-table.js";
import { renderBulkActionsBar } from "../components/model-bulk-actions.js";
import { statusPillClass } from "../lib/constants.js";
import type { Model } from "../lib/types/api.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";

interface TestResult {
  status: number;
  elapsed_ms: number;
  row_id?: number;
}

// ===== Edit model (legacy) =====
//
// The legacy "Edit model" modal is preserved for backwards
// compatibility — older UI surfaces still call it. New code
// should use the in-table Enable/Disable buttons instead.

export async function showEditModel(rowId: number): Promise<void> {
  if (!state.models || state.models.length === 0) state.models = await api("/models") as Model[];
  const m = (state.models || []).find((x) => x.row_id === rowId);
  if (!m) { showToast("Model row not found", "error"); return; }
  const html = `
    <div class="modal-bg" id="edit-model-modal" data-action="closeModalBg" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>Edit model row #${rowId}</h2>
          <button type="button" class="close-btn" data-action="closeModalBg" data-arg1="self" aria-label="Close">&times;</button>
        </div>
        <form data-action="updateModel" data-arg1="${rowId}">
          <div class="modal-body">
            <div class="field">
              <label>Model id</label>
              <input name="model_id" type="text" value="${escapeAttr(m.model_id || "")}" required>
            </div>
            <div class="field">
              <label>Display name</label>
              <input name="display_name" type="text" value="${escapeAttr(m.display_name || "")}">
            </div>
            <div class="field">
              <label>Active</label>
              <select name="active">
                <option value="true" ${m.active ? "selected" : ""}>yes</option>
                <option value="false" ${!m.active ? "selected" : ""}>no</option>
              </select>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" data-action="closeModalBg" data-arg1="self">Cancel</button>
            <button type="submit" class="primary">Save</button>
          </div>
        </form>
      </div>
    </div>
  `;
  // Mount on <body> via appendModal (not #main) so the 3s background
  // poll doesn't destroy the form mid-edit. See lib/dom.ts appendModal.
  appendModal(html);
}

export async function updateModel(rowId: number, e: Event): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const body = {
    model_id: f.get("model_id"),
    display_name: f.get("display_name") || null,
    active: f.get("active") === "true",
  };
  try {
    await api("/models/" + rowId, { method: "PATCH", body: JSON.stringify(body) });
    state.models = await api("/models") as Model[];
    const modalBg = target.closest(".modal-bg");
    if (modalBg) modalBg.remove();
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

// ===== Per-row model handlers =====

// Soft-disable / re-enable a single model. The row's id is the
// server-side numeric primary key (NOT the upstream model id).
export async function toggleModel(rowId: number, newActive: boolean | unknown, e: Event | null): Promise<void> {
  // The data-action shim passes the event as the last arg. We
  // accept a boolean for newActive (from data-arg2) or fall back
  // to the event target's checked state for compatibility.
  const desired: boolean = typeof newActive === "boolean" ? newActive
    : !!(e && e.target && e.target instanceof HTMLInputElement ? e.target.checked : false);
  try {
    await api("/models/" + rowId + "/toggle", {
      method: "POST",
      body: JSON.stringify({ active: desired }),
    });
    const m = (state.models || []).find((x) => x.row_id === rowId);
    if (m) m.active = desired;
    // Targeted DOM patch — sync the row's active-state UI in
    // place (row class, status pill, Enable/Disable button). We
    // do NOT call requestUpdate() because a full rebuild
    // would close any open `<select>` (filter tabs, provider
    // dropdown) and steal focus from the search input the user
    // may still be editing. Mirrors patchComboField in
    // combo-handlers.ts.
    syncModelRowActive(rowId, desired);
    // Refresh the (All / Active / Inactive) counts on the filter
    // tabs so the totals reflect the new state.
    const ctx = state.currentView && state.currentView.context;
    if (ctx) {
      const allProviderModels = (state.models || []).filter((mm) => mm.provider_id === ctx);
      updateFilterTabCounts(ctx, allProviderModels);
    }
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

// Fire a single test request against the upstream for one model.
// We only re-render the affected row's "last test" cell — there's
// no need to redraw the whole table for a 50ms latency stamp.
// The button itself gets a coloured flash so the click feels
// acknowledged even when the request takes a few seconds.
export async function testModel(rowId: number, _modelId: string, _e: Event | null): Promise<void> {
  const btn = document.getElementById(`test-btn-${rowId}`) as HTMLButtonElement | null;
  if (!btn) return;
  const oldText = btn.textContent;
  btn.disabled = true;
  btn.textContent = "Testing...";
  try {
    const result = (await api(`/models/${rowId}/test`, { method: "POST" })) as TestResult;
    // Update only the "last test" cell so we don't lose the
    // user's scroll / focus on a 200-row table. The row id is
    // set in the server response; fall back to the request rowId
    // if the server omits it (older builds).
    const rid = result.row_id ?? rowId;
    const row = document.getElementById(`model-row-${rid}`);
    if (row) {
      // The "Last test" cell is the 8th child (0-indexed = 8)
      // of the row: checkbox, Model ID, Display, Format, Context,
      // Out, Capabilities, Status, Last test, Actions. We use the
      // class selector rather than the index because the latter
      // is brittle to column reorders.
      const cell = row.querySelector(".last-test-cell");
      if (cell) {
        cell.innerHTML = `<span class="status-pill ${statusPillClass(result.status)}">${result.status}</span> <small>${result.elapsed_ms}ms</small>`;
      }
    }
    if (result.status >= 200 && result.status < 300) {
      flashButton(btn, "✓", "#a6e3a1");
    } else if (result.status === 0) {
      flashButton(btn, "✗ net", "#f38ba8");
    } else {
      flashButton(btn, "✗ " + result.status, "#f38ba8");
    }
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

// Brief button colour flash. Same shape as the one in
// provider-handlers.ts — duplicated here to keep the modules
// dependency-free.
function flashButton(btn: HTMLButtonElement | null, text: string, color: string): void {
  if (!btn) return;
  btn.textContent = text;
  btn.style.background = color;
  setTimeout(() => { btn.style.background = ""; }, 1500);
}

export async function deleteModel(rowId: number): Promise<void> {
  if (!confirm("Delete this model? Combo targets referencing it will be removed too.")) return;
  try {
    await api(`/models/${rowId}`, { method: "DELETE" });
    state.models = state.models.filter((m) => m.row_id !== rowId);
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

// ===== Selection (multi-select) =====
//
// The selection is a Set of model row_ids. It is cleared at the
// top of `renderProviderDetail` so a navigation between
// providers never leaks selections across providers. The bulk-
// actions bar and the per-row `tr.selected` class both re-derive
// from the Set on every render, so the only mutation points
// are these four functions.

export function toggleModelSelection(rowId: number, e: Event | null): void {
  const target = e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  if (checked) state.selectedModels.add(rowId);
  else state.selectedModels.delete(rowId);
  // Don't full re-render here: just toggle the row's `selected`
  // class and update the bulk bar. The row id is known so we can
  // do a targeted DOM patch in O(1).
  const row = document.getElementById(`model-row-${rowId}`);
  if (row) row.classList.toggle("selected", checked);
  updateBulkBar();
  // Sync the master "select all" checkbox state. The DOM
  // mutation is cheap; we re-read the visible row_ids to compute
  // the new indeterminate state.
  const visible = getVisibleModelRowIds();
  syncSelectAllCheckbox(visible);
}

// Toggle every row currently passing the active/inactive filter
// + search box, not every model of the provider. This is what
// the "select all" affordance promises: a 200-row provider where
// 3 rows match the user's search shouldn't surprise them by
// selecting 197 extra rows.
export function toggleSelectAllModels(e: Event | null): void {
  const target = e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  const visible = getVisibleModelRowIds();
  if (checked) {
    for (const id of visible) state.selectedModels.add(id);
  } else {
    for (const id of visible) state.selectedModels.delete(id);
  }
  // Targeted DOM patch — toggle each visible row's `selected`
  // class and refresh the bulk-action bar. The master checkbox
  // is already toggled by the browser. We do NOT call
  // requestUpdate() because a full rebuild would close any
  // open `<select>` (filter tabs, provider dropdown) and steal
  // focus from the search input the user may still be editing.
  // Mirrors patchComboField in combo-handlers.ts.
  for (const id of visible) {
    const row = document.getElementById(`model-row-${id}`);
    if (row) row.classList.toggle("selected", checked);
  }
  updateBulkBar();
}

export function clearModelSelection(): void {
  state.selectedModels.clear();
  // Targeted DOM patch: uncheck every visible checkbox, drop every
  // row's `selected` class, hide the bulk-action bar, and reset
  // the master "select all" checkbox. A full re-render would
  // close any open `<select>` on the page and steal focus from
  // the search input; the targeted patch preserves the rest of
  // the DOM. Mirrors patchComboField in combo-handlers.ts.
  document.querySelectorAll<HTMLInputElement>(
    '#models-tbody input[type="checkbox"]'
  ).forEach((cb) => { cb.checked = false; });
  document.querySelectorAll("tr[id^='model-row-'].selected").forEach((row) => {
    row.classList.remove("selected");
  });
  updateBulkBar();
  syncSelectAllCheckbox([]);
}

// Re-render the bulk-action bar with the current count. Cheaper
// than a full re-render — we only touch the bar's "N selected"
// counter, then re-paint the bar so its buttons (which don't
// change) are intact.
//
// We import the bar template lazily to avoid a circular import
// (model-bulk-actions.js doesn't import this file, but a static
// import here would be hoisted to the top of the module and
// components → state → handler dependencies don't actually form
// a cycle, so the dynamic import is overkill — we use a static
// import at the top of the file).
function updateBulkBar(): void {
  const tbody = document.getElementById("models-tbody");
  if (!tbody) return;
  const section = tbody.closest("section");
  if (!section) return;
  // The bar lives in the same <section> as the tbody. We
  // update its count in place if it exists, or insert it before
  // the table on the first paint. Always re-query the section
  // to avoid stale references between re-renders.
  let bar = section.querySelector(".bulk-actions-bar") as HTMLElement | null;
  const count = state.selectedModels.size;
  if (count === 0) {
    if (bar) {
      // Remove the bar (and its wrapper <div> if one was
      // inserted by this function on a previous call). When
      // the bar was rendered inline by the parent view, its
      // parentElement is the <section> itself — we leave the
      // section alone.
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
    // Render the bulk-actions bar TemplateResult into a fresh
    // wrapper <div> inserted just before the table. lit-html
    // replaces the wrapper's children with the bar markup.
    const table = section.querySelector("table");
    if (table) {
      const wrapper = document.createElement("div");
      table.insertAdjacentElement("beforebegin", wrapper);
      render(renderBulkActionsBar(providerId), wrapper);
    }
  }
}

// ===== Bulk enable / disable / test / delete =====

async function bulkSetSelected(_providerId: string, active: boolean): Promise<void> {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!confirm(`${active ? "Enable" : "Disable"} ${ids.length} models?`)) return;
  // Per-row toggle in parallel: each toggle is its own atomic
  // UPDATE on the server. The previous bulk-toggle endpoint
  // applied to *all* non-custom rows of the provider, which is
  // exactly the over-broad behavior the per-row selection is
  // meant to escape.
  await Promise.all(ids.map((rowId) =>
    api("/models/" + rowId + "/toggle", {
      method: "POST",
      body: JSON.stringify({ active }),
    }).catch((err: unknown) => console.error("Failed toggle", rowId, err))
  ));
  state.models = await api("/models") as Model[];
  // Targeted DOM patch — for each toggled row, sync the
  // active-state UI in place. Clear the selection (uncheck all,
  // remove `selected` classes, hide the bulk bar, reset master
  // checkbox). We do NOT call requestUpdate() — a full
  // rebuild would close any open `<select>` and steal focus from
  // the search input. Mirrors patchComboField in combo-handlers.ts.
  for (const rowId of ids) {
    const rid = Number(rowId);
    if (!Number.isFinite(rid)) continue;
    const m = (state.models || []).find((x) => x.row_id === rid);
    if (m) syncModelRowActive(rid, m.active);
  }
  state.selectedModels.clear();
  document.querySelectorAll<HTMLInputElement>(
    '#models-tbody input[type="checkbox"]'
  ).forEach((cb) => { cb.checked = false; });
  document.querySelectorAll("tr[id^='model-row-'].selected").forEach((row) => {
    row.classList.remove("selected");
  });
  updateBulkBar();
  syncSelectAllCheckbox([]);
  // Refresh the (All / Active / Inactive) counts on the filter
  // tabs so the totals reflect the new state.
  const ctx = state.currentView && state.currentView.context;
  if (ctx) {
    const allProviderModels = (state.models || []).filter((mm) => mm.provider_id === ctx);
    updateFilterTabCounts(ctx, allProviderModels);
  }
}

export function bulkEnableSelected(_providerId: string): Promise<void> { return bulkSetSelected(_providerId, true); }
export function bulkDisableSelected(_providerId: string): Promise<void> { return bulkSetSelected(_providerId, false); }

export async function bulkTestSelected(_providerId: string): Promise<void> {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!confirm(`Test ${ids.length} models sequentially?`)) return;
  for (const rowId of ids) {
    try {
      const btn = document.getElementById(`test-btn-${rowId}`) as HTMLButtonElement | null;
      if (btn) {
        btn.disabled = true;
        btn.textContent = "Testing...";
      }
      const result = (await api(`/models/${rowId}/test`, { method: "POST" })) as TestResult;
      const row = document.getElementById(`model-row-${rowId}`);
      if (row) {
        const cell = row.querySelector(".last-test-cell");
        if (cell) {
          cell.innerHTML = `<span class="status-pill ${statusPillClass(result.status)}">${result.status}</span> <small>${result.elapsed_ms}ms</small>`;
        }
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
    } catch (err: unknown) {
      console.error("Test failed", rowId, err);
    }
  }
  // Refresh the models cache so the background poll is a no-op
  // and the next render shows the up-to-date last_test_* columns.
  state.models = await api("/models") as Model[];
}

export async function bulkDeleteSelected(_providerId: string): Promise<void> {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!confirm(`Delete ${ids.length} models? This cannot be undone.`)) return;
  await Promise.all(ids.map((rowId) =>
    api("/models/" + rowId, { method: "DELETE" })
      .catch((err: unknown) => console.error("Failed delete", rowId, err))
  ));
  state.models = await api("/models") as Model[];
  state.selectedModels.clear();
  requestUpdate();
}

// ===== Filter / search =====

// Update the per-provider search/filter state and re-render only
// the affected parts (the model tbody + the filter-tab counts).
// A full re-render of renderProviderDetail would replace the
// search input itself and steal focus mid-keystroke, so we keep
// the surrounding DOM stable and patch the tbody in place. The
// search input keeps focus because we never remove it from the
// document.
//
// Argument order: the data-action shim passes data-arg-N
// followed by the event. The search input declares data-arg1
// (provider id) and data-arg2 (the state key — "search" or
// "filter"); the *new value* is read off e.target.value because
// it's the live value of the input the user is editing. The
// filter tabs read their value from data-arg3.
export function updateProviderFilter(providerId: string, key: string, valueFromArg3: unknown, event: Event | null): void {
  // The shim passes positional data-args then the event last.
  // For the search input there is no data-arg3, so the 3rd arg
  // is the event itself. For the filter tabs data-arg3 holds
  // the value ("all" / "active" / "inactive"). We branch on
  // whether the 3rd arg looks like an Event.
  let value: string;
  if (key === "filter") {
    // Filter tab: value comes from data-arg3 ("all", "active",
    // "inactive").
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
    // Search input: value is the live text in the input.
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
    // Backfill the `sort` field for providers visited before the
    // sortable-headers feature landed.
    state.providerDetail[providerId]["sort"] = null;
  }
  state.providerDetail[providerId][key] = value;
  const ui = state.providerDetail[providerId] as { filter: string; search: string; sort: unknown };

  // Recompute the visible models from the same rules used by
  // renderProviderDetail. Keeping the logic in one place would
  // require a `filterModels(providerId)` helper, but it's three
  // conditions and the duplication is clearer than the indirection.
  const searchLower = (ui.search || "").toLowerCase();
  const allProviderModels = (state.models || []).filter((m) => m.provider_id === providerId);
  const filtered = allProviderModels.filter((m) => {
    if (ui.filter === "active" && !m.active) return false;
    if (ui.filter === "inactive" && m.active) return false;
    if (searchLower && !m.model_id.toLowerCase().includes(searchLower)) return false;
    return true;
  });
  // Apply the same sort the full render uses, so a filter change
  // doesn't reset the user's chosen column ordering.
  const sorted = applySort(filtered, ui.sort as Parameters<typeof applySort>[1]);

  // Re-paint the tbody (and its empty-state row) without touching
  // the surrounding page chrome. The search input lives outside
  // the tbody, so its focus survives. lit-html diffs the new
  // TemplateResult against the previous render so only the
  // changed rows are patched.
  const tbody = document.getElementById("models-tbody");
  if (tbody) {
    render(
      sorted.length === 0
        ? html`<tr><td colspan="10" class="empty-row">No models match the filter.</td></tr>`
        : renderModelRows(sorted),
      tbody
    );
  }

  // Refresh the (All / Active / Inactive) counts on the filter
  // tabs. The numbers don't change as the user types, but
  // keeping them in sync via a single updater means we don't
  // have to remember to also update them when the data shape
  // evolves.
  updateFilterTabCounts(providerId, allProviderModels);

  // The master "select all" checkbox state depends on which rows
  // are currently visible (see the note in renderProviderDetail).
  // The full re-render ran this in a queueMicrotask; we run it
  // now because the microtask queue won't be flushed on a
  // partial paint.
  syncSelectAllCheckbox(sorted.map((m) => m.row_id));
}

// Persist the provider's auto-activate keyword. The user types
// and tabs out (or clicks away); `change` fires once. The data-
// action dispatcher also fires on every `input` keystroke, so we
// filter on `e.type === "input"` to avoid PATCHing on every key.
// The endpoint takes a three-state `null` / string — we send
// `null` for an empty input to clear the column back to NULL so a
// future refresh re-enables *all* non-custom models.
export async function updateAutoActivate(providerId: string, e: Event | null): Promise<void> {
  // Only fire on "change" (blur/enter), not on every "input"
  // keystroke — same guard as `updateTargetWeight` etc.
  if (e && e.type === "input") return;
  const target = e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const value = target ? target.value : "";
  const body = { auto_activate_keyword: value && value.trim() ? value.trim() : null };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    });
    // Refresh the providers cache so the next background-poll
    // diff is a no-op and the input value (in case the server
    // normalized the string) reflects the truth.
    state.providers = await api("/providers") as typeof state.providers;
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
    // Don't re-render on error — see patchComboField in
    // combo-handlers.ts for the rationale. The user's input
    // already shows their text; a re-render would close any
    // other open input on the page and steal focus.
  }
}

// ===== Custom model form =====
//
// Re-exported from components/model-custom-form.js so the data-
// action shim has a single place to find them.

export { showCustomModelForm, closeCustomModelForm } from "../components/model-custom-form.js";

// POST /admin/models/custom — hand-create a model row. The
// server stamps the row with `custom = 1` and `active = 1` so
// it's routable as soon as the modal closes. We do the close-
// modal-then-refetch dance to avoid the re-render of the parent
// clobbering the modal mid-close.
export async function createCustomModel(providerId: string, e: Event): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const body = {
    provider_id: providerId,
    model_id: f.get("model_id"),
    display_name: f.get("display_name") || null,
    target_format: f.get("target_format"),
    ttl_seconds: parseInt(String(f.get("ttl_seconds"))) || 0,
  };
  try {
    await api("/models/custom", { method: "POST", body: JSON.stringify(body) });
    const modalBg = target.closest(".modal-bg");
    if (modalBg) modalBg.remove();
    state.models = await api("/models") as Model[];
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

// Cycle the sort state for a column on the models table. The
// signature matches the `data-action="cycleProviderSort"` shim:
// arg1 = providerId, arg2 = sortKey, then the event. We compute
// the new (key, dir) tuple from the previous state:
//
//   no sort          → sort by this column, asc
//   this column asc  → this column desc
//   this column desc → no sort (back to upstream order)
//
// The full re-render is needed (not just a tbody paint) because
// the <th> indicators have to flip too, and the partial-paint
// helper only re-renders rows.
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
