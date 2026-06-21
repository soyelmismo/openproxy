// components/model-table.ts — render the inner HTML of the
// provider-detail models table. Pulled out of the old monolithic
// renderProviderDetail() so updateProviderFilter() can re-paint
// just the rows (the search input lives outside the tbody, so its
// focus survives the partial re-paint).
//
// All exports are pure functions of `(state, props)`. They mutate
// only the DOM via the `innerHTML` setter on the tbody — never the
// `state` singleton.

import { state } from "../state/index.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { statusPillClass } from "../lib/constants.js";
import type { Model } from "../lib/types/api.js";

// Map an HTTP status code to a status-pill CSS class. The server
// stamps `0` when the request never reached the upstream (DNS /
// connect / TLS / timeout); treat it as the red "off" pill so it
// reads as a network error at a glance.
export function modelStatusPillClass(status: number | null): string {
  if (status == null) return "off";
  if (status === 0) return "off";
  if (status >= 200 && status < 300) return "on";
  if (status >= 400 && status < 500) return "warn";
  if (status >= 500) return "off";
  return "";
}

// Format a token count for compact display. null/undefined render
// as an em-dash (with the muted class) so the column stays the
// same width across rows. Anything above 1k uses `k`; above 1M
// uses `M` with one decimal.
export function formatContext(tokens: number | null | undefined): string {
  if (tokens == null) return '<span class="muted">—</span>';
  if (tokens >= 1000000) return (tokens / 1000000).toFixed(1) + "M";
  if (tokens >= 1000) return Math.round(tokens / 1000) + "k";
  return String(tokens);
}

// Render the per-model capability badges (vision/tools/reasoning/…).
// Accepts either a JSON string (the wire shape from /admin/models)
// or a plain object (in case a caller pre-parsed it). Bad input
// renders as an em-dash rather than throwing — the admin list should
// never blow up because of a single bad row.
export function renderCapabilityBadges(json: string | null | undefined): string {
  if (json == null) return '<span class="muted">—</span>';
  let caps: unknown;
  if (typeof json === "string") {
    try { caps = JSON.parse(json) as unknown; } catch (_e: unknown) { return '<span class="muted">—</span>'; }
  } else {
    caps = json;
  }
  if (!caps || typeof caps !== "object") return '<span class="muted">—</span>';
  const c: Record<string, unknown> = caps as Record<string, unknown>;
  const badges: string[] = [];
  if (c["vision"]) badges.push('<span class="cap-badge">vision</span>');
  if (c["tool_calling"]) badges.push('<span class="cap-badge">tools</span>');
  if (c["reasoning"]) badges.push('<span class="cap-badge">reasoning</span>');
  if (c["thinking"]) badges.push('<span class="cap-badge">thinking</span>');
  if (c["structured_output"]) badges.push('<span class="cap-badge">json</span>');
  if (c["attachment"]) badges.push('<span class="cap-badge">attach</span>');
  return badges.length > 0 ? badges.join(" ") : '<span class="muted">—</span>';
}

// Build a single <tr> for a model row. The caller passes the
// already-filtered model object. The row id is the server-side
// `row_id` (numeric primary key) — the /admin/models/:id/...
// endpoints key off that.
export function renderModelRow(m: Model): string {
  const lastTest: string = m.last_test_status != null
    ? `<span class="status-pill ${statusPillClass(m.last_test_status)}">${escapeHtml(String(m.last_test_status))}</span> <small>${escapeHtml(m.last_test_at || "")}</small>`
    : '<span class="muted">never</span>';
  const isSelected: boolean = (state.selectedModels as Set<number>).has(m.row_id);
  return `
    <tr id="model-row-${m.row_id}" class="${m.active ? "" : "inactive"} ${isSelected ? "selected" : ""}">
      <td><input type="checkbox" ${isSelected ? "checked" : ""} data-action="toggleModelSelection" data-arg1="${m.row_id}"></td>
      <td><code>${escapeHtml(m.model_id)}</code>${m.custom ? '<span class="badge custom">custom</span>' : ""}</td>
      <td>${escapeHtml(m.display_name || "—")}</td>
      <td>${escapeHtml(m.target_format || "—")}</td>
      <td>${formatContext(m.context_length)}</td>
      <td>${formatContext(m.max_output_tokens)}</td>
      <td>${renderCapabilityBadges(m.capabilities_json)}${m.family ? ` <small class="muted">${escapeHtml(m.family)}</small>` : ""}</td>
      <td><span class="status-pill ${m.active ? "on" : "off"}">${m.active ? "active" : "inactive"}</span></td>
      <td class="last-test-cell">${lastTest}</td>
      <td>
        <button class="small" id="test-btn-${m.row_id}" data-action="testModel" data-arg1="${m.row_id}" data-arg2="${escapeAttr(m.model_id)}">Test</button>
        <button class="small" data-action="toggleModel" data-arg1="${m.row_id}" data-arg2="${!m.active}">${m.active ? "Disable" : "Enable"}</button>
        <button class="small danger" data-action="deleteModel" data-arg1="${m.row_id}">×</button>
      </td>
    </tr>
  `;
}

// Concatenate the row HTML for an array of model rows. Caller
// supplies the pre-filtered list (e.g. search + active/inactive
// filter already applied).
export function renderModelRows(rows: readonly Model[]): string {
  let html: string = "";
  for (const m of rows) html += renderModelRow(m);
  return html;
}

// Apply the per-provider search+filter state to the global models
// cache and return the row_ids of the visible models. Used by
// `toggleSelectAllModels` so the master "select all" checkbox only
// catches the rows the user can actually see.
export function getVisibleModelRowIds(): number[] {
  if (!state.currentView || state.currentView.context == null) return [];
  const providerId: string = state.currentView.context;
  const ui: Record<string, unknown> | undefined = (state.providerDetail as Record<string, Record<string, unknown>>)[providerId];
  if (!ui) return [];
  const search: string = (typeof ui["search"] === "string" ? (ui["search"] as string) : "").toLowerCase();
  const filter: string = typeof ui["filter"] === "string" ? (ui["filter"] as string) : "";
  return state.models
    .filter((m) => m.provider_id === providerId)
    .filter((m) => {
      if (filter === "active" && !m.active) return false;
      if (filter === "inactive" && m.active) return false;
      if (search && !m.model_id.toLowerCase().includes(search)) return false;
      return true;
    })
    .map((m) => m.row_id);
}

// Rewrite the (All / Active / Inactive) counts on the filter tabs
// so the user sees the totals for the provider, not for the
// current filter. Cheaper than a full re-render.
export function updateFilterTabCounts(providerId: string, allProviderModels: readonly Model[]): void {
  const active: number = allProviderModels.filter((m) => m.active).length;
  const inactive: number = allProviderModels.length - active;
  const allBtn: HTMLElement | null = document.getElementById(`filter-tab-${providerId}-all`);
  const activeBtn: HTMLElement | null = document.getElementById(`filter-tab-${providerId}-active`);
  const inactiveBtn: HTMLElement | null = document.getElementById(`filter-tab-${providerId}-inactive`);
  if (allBtn) allBtn.textContent = `All (${allProviderModels.length})`;
  if (activeBtn) activeBtn.textContent = `Active (${active})`;
  if (inactiveBtn) inactiveBtn.textContent = `Inactive (${inactive})`;
}

// Sync the master "select all" checkbox state with the in-flight
// selection: checked if all visible rows are selected,
// indeterminate if some, unchecked if none. Used by both the
// initial render and the partial re-paint in updateProviderFilter.
export function syncSelectAllCheckbox(visibleRowIds: readonly number[]): void {
  const master: HTMLInputElement | null = document.getElementById("model-select-all") as HTMLInputElement | null;
  if (!master) return;
  if (visibleRowIds.length === 0) {
    master.checked = false;
    master.indeterminate = false;
    return;
  }
  const selectedVisible: number = visibleRowIds.filter((id) => (state.selectedModels as Set<number>).has(id)).length;
  if (selectedVisible === 0) {
    master.checked = false;
    master.indeterminate = false;
  } else if (selectedVisible === visibleRowIds.length) {
    master.checked = true;
    master.indeterminate = false;
  } else {
    master.checked = false;
    master.indeterminate = true;
  }
}

// ---- Column sorting ------------------------------------------------------
//
// The user can click any header in the models table to sort by that
// column. Each click cycles through three states:
//
//   none → asc → desc → none → asc → ...
//
// "none" restores the original upstream order (which is itself
// meaningful — the rows came back in the same order the provider
// returned them, e.g. family groupings from OpenRouter). The active
// state is persisted per-provider in `state.providerDetail[id].sort`
// so a navigation away and back doesn't lose the user's choice.
//
// The indicator (▲/▼) is rendered inline in the <th> as a Unicode
// arrow next to the column label; empty cells mean "not sorted".
// Sortable columns get the `sortable` CSS class (cursor: pointer +
// hover background) so the affordance is obvious.

export interface SortableColumn {
  key: string;
  label: string;
  value: (m: Model) => string | number;
}

export const SORTABLE_COLUMNS: readonly SortableColumn[] = [
  // key matches `data-sort-key`; label is the human text; value
  // is the extractor for the model row. `null` extractors mean
  // "stable" (the upstream order is preserved).
  { key: "model_id",   label: "Model ID",   value: (m) => (m.model_id || "").toLowerCase() },
  { key: "display",    label: "Display",    value: (m) => (m.display_name || "").toLowerCase() },
  { key: "format",     label: "Format",     value: (m) => (m.target_format || "").toLowerCase() },
  { key: "context",    label: "Context",    value: (m) => m.context_length || 0 },
  { key: "out",        label: "Out",        value: (m) => m.max_output_tokens || 0 },
];

export interface ModelSort {
  key: string;
  dir: "asc" | "desc" | string;
}

// Apply the per-provider sort (if any) to the filtered row list.
// Returns a new array; the input is not mutated. `null`/missing
// sort state returns the input unchanged.
export function applySort(rows: readonly Model[], sort: ModelSort | null): readonly Model[] {
  if (!sort || !sort.key) return rows;
  const col: SortableColumn | undefined = SORTABLE_COLUMNS.find((c) => c.key === sort.key);
  if (!col) return rows;
  const dir: number = sort.dir === "desc" ? -1 : 1;
  // Stable sort: when two rows compare equal, keep their original
  // relative order. Array.prototype.sort is stable in modern V8.
  const out: Model[] = rows.slice();
  out.sort((a, b) => {
    const va: string | number = col.value(a);
    const vb: string | number = col.value(b);
    if (va < vb) return -1 * dir;
    if (va > vb) return 1 * dir;
    return 0;
  });
  return out;
}

// Render the <th> for a sortable column with the right indicator
// and data attributes the click handler reads. `sort` is the
// per-provider sort state (or null for unsorted).
export function renderSortableTh(col: SortableColumn, sort: ModelSort | null, providerId: string): string {
  const isActive: boolean = !!(sort && sort.key === col.key);
  const indicator: string = isActive ? (sort && sort.dir === "desc" ? " ▼" : " ▲") : "";
  return `<th class="sortable${isActive ? " sorted" : ""}" data-action="cycleProviderSort" data-arg1="${escapeAttr(providerId)}" data-arg2="${escapeAttr(col.key)}">${escapeHtml(col.label)}<span class="sort-indicator">${indicator}</span></th>`;
}
